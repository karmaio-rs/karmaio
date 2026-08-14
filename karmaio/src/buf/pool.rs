//! Runtime-provided buffer pool for io_uring buffer selection.
//!
//! # Ownership model
//!
//! [`BufferPool`] is a handle to the current runtime's provided-buffer ring
//! (one pool per local runtime / driver on Linux). [`PooledBuf`] is a
//! **temporary lease** on one slot from that pool:
//!
//! - You may read/write the leased bytes while you hold the value.
//! - Dropping the value (or calling [`PooledBuf::release`]) returns the
//!   slot to the pool so the kernel can use it again.
//! - You do **not** own the underlying allocation in the usual sense; holding
//!   many leases without recycling will exhaust the pool (`ENOBUFS` on
//!   managed / multishot receives).
//!
//! # Buffer leak / starvation risks
//!
//! - **Hold times:** every outstanding [`PooledBuf`] is one buffer the
//!   kernel cannot select. Multishot recv ends with `ENOBUFS` when the ring
//!   is empty; recycle promptly after parsing. The pool is runtime-global, so
//!   one slow socket can starve unrelated sockets on the same runtime.
//! - **Cancel / drop paths:** the runtime returns undelivered selected
//!   buffers when a multishot stream is dropped. Application code must still
//!   drop or [`release`](PooledBuf::release) every buffer it received.
//! - **Runtime shutdown:** if a [`PooledBuf`] outlives the runtime, its
//!   drop frees the allocation instead of returning it to the ring. Avoid
//!   storing leases past `block_on` / runtime teardown.
//!
//! Requires Linux 6.12+ for the intended managed / multishot consumers.
//! karmaio does not probe the kernel version.

#![cfg(target_os = "linux")]

use std::{
    cell::UnsafeCell,
    fmt, io,
    mem::{self, ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    rc::{Rc, Weak},
    slice,
    sync::atomic::{AtomicU16, Ordering},
};

use io_uring::{IoUring, types::BufRingEntry};

use crate::buf::{IoBuf, IoBufMut, SetLen};

/// Buffer group id registered with the driver's io_uring instance.
pub(crate) const BUFFER_GROUP_ID: u16 = 1;
/// Maximum number of entries accepted by an io_uring provided-buffer ring.
pub(crate) const MAX_BUFFER_RING_ENTRIES: u16 = 1 << 15;

/// Driver-internal handle to the runtime's provided buffer pool.
///
/// The handle is neither [`Send`] nor [`Sync`]: it is bound to the local
/// runtime thread that registered the ring.
#[derive(Clone)]
pub(crate) struct BufferPool {
    shared: Weak<Shared>,
}

/// One leased buffer from the runtime's io_uring provided-buffer pool.
///
/// Dropping the value (or calling [`release`](Self::release)) recycles its slot.
/// Holding many leases can exhaust the pool and make managed receives fail with
/// `ENOBUFS`. Implements [`IoBuf`] / [`IoBufMut`] so leases can be passed to
/// write paths and codecs that already accept those traits.
pub struct PooledBuf {
    /// Weak handle of the pool used for recycle on drop.
    shared: Weak<Shared>,
    /// Pointer to the leased buffer memory.
    ptr: NonNull<u8>,
    /// Start of the application-visible view within the allocation.
    offset: u32,
    /// Initialized byte count (set after a successful receive).
    len: u32,
    /// Writable capacity exposed to the application (≤ full capacity).
    cap: u32,
    /// Full allocation size; used if the pool is gone and we free ourselves.
    full_cap: u32,
    /// Index of this slot in the pool.
    buffer_id: u16,
}

struct Shared {
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// mmap'd buf-ring entries shared with the kernel.
    ring: BufRing,
    /// Slot table: `None` while leased to userspace / kernel.
    bufs: Vec<Option<NonNull<u8>>>,
    /// Bytes per buffer.
    size: u32,
}

/// An mmap allocation that is not currently registered with the kernel.
struct RingMapping {
    ptr: NonNull<BufRingEntry>,
    map_len: usize,
}

impl RingMapping {
    fn new(map_len: usize) -> io::Result<Self> {
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(raw.cast::<BufRingEntry>()).expect("MAP_FAILED already checked"),
            map_len,
        })
    }

    fn unmap(&mut self) -> io::Result<()> {
        let rc = unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.map_len) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        self.ptr = NonNull::dangling();
        self.map_len = 0;
        Ok(())
    }
}

impl Drop for RingMapping {
    fn drop(&mut self) {
        if self.map_len != 0 {
            let _ = self.unmap();
        }
    }
}

/// A kernel-registered provided-buffer ring.
struct BufRing {
    // Registered mappings must not be unmapped from Drop. Successful
    // unregister takes this value out and restores ordinary mapping RAII.
    mapping: ManuallyDrop<RingMapping>,
    len: u16,
}

enum BufRingRelease {
    Released(io::Result<()>),
    StillRegistered(io::Error),
}

impl BufRing {
    /// # Safety
    ///
    /// `bufs` must stay valid and unused by userspace until the corresponding
    /// buffer id is returned from a CQE and taken out of the pool.
    unsafe fn register(uring: &IoUring, bufs: &[Option<NonNull<u8>>], buf_len: u32, flags: u16) -> io::Result<Self> {
        debug_assert!(bufs.len().is_power_of_two());
        let len = u16::try_from(bufs.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "buffer pool size exceeds u16::MAX"))?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer pool size must be non-zero",
            ));
        }

        let map_len = bufs.len() * mem::size_of::<BufRingEntry>();
        // page-aligned anonymous mapping as required for provided buffer rings
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = if page <= 0 { 4096 } else { page as usize };
        let map_len = map_len.div_ceil(page) * page;

        let mapping = RingMapping::new(map_len)?;

        unsafe {
            uring
                .submitter()
                .register_buf_ring_with_flags(mapping.ptr.as_ptr() as u64, len, BUFFER_GROUP_ID, flags)?;
        }
        let mut ring = Self {
            mapping: ManuallyDrop::new(mapping),
            len,
        };

        for (id, slot) in bufs.iter().enumerate() {
            let buf = slot.expect("pool initialized with null buffer slots");
            let id = id as u16;
            unsafe { ring.add_buffer(id, buf, buf_len, id) };
        }
        unsafe { ring.commit(len) };

        Ok(ring)
    }

    fn as_slice_mut(&mut self) -> &mut [BufRingEntry] {
        // Safety: mmap zero-fills; BufRingEntry is plain data.
        unsafe { slice::from_raw_parts_mut(self.mapping.ptr.as_ptr(), self.len as usize) }
    }

    fn tail(&self) -> &AtomicU16 {
        // Safety: first entry's resv field is the ring tail (liburing layout).
        unsafe { &*BufRingEntry::tail(self.mapping.ptr.as_ptr()).cast::<AtomicU16>() }
    }

    /// Stage one buffer; pair with [`commit`].
    ///
    /// # Safety
    /// Buffer must remain valid until taken from a CQE or the ring is unregistered.
    unsafe fn add_buffer(&mut self, buffer_id: u16, ptr: NonNull<u8>, len: u32, offset: u16) {
        let idx = self.tail().load(Ordering::Acquire).wrapping_add(offset) % self.len;
        let entry = &mut self.as_slice_mut()[idx as usize];
        entry.set_addr(ptr.as_ptr() as u64);
        entry.set_len(len);
        entry.set_bid(buffer_id);
    }

    /// Publish `count` staged buffers to the kernel.
    ///
    /// # Safety
    /// Entries in the range must be valid and not in use.
    unsafe fn commit(&self, count: u16) {
        self.tail().fetch_add(count, Ordering::Release);
    }

    unsafe fn reset_buffer(&mut self, buffer_id: u16, ptr: NonNull<u8>, len: u32) {
        unsafe {
            self.add_buffer(buffer_id, ptr, len, 0);
            self.commit(1);
        }
    }

    unsafe fn unregister(mut self, uring: &IoUring) -> BufRingRelease {
        if let Err(error) = uring.submitter().unregister_buf_ring(BUFFER_GROUP_ID) {
            // `self` drops here. Its destructor deliberately retains the
            // mapping because the kernel may still reference it.
            return BufRingRelease::StillRegistered(error);
        }

        // Safety: unregister succeeded, so the kernel no longer references the
        // mapping. `BufRing::drop` deliberately does not touch this field.
        let mut mapping = unsafe { ManuallyDrop::take(&mut self.mapping) };
        let result = mapping.unmap();
        drop(mapping);
        BufRingRelease::Released(result)
    }
}

impl Drop for BufRing {
    fn drop(&mut self) {
        // A value of this type is registered by construction. If explicit
        // unregister did not succeed, leak its mapping rather than let the
        // kernel dereference unmapped memory.
    }
}

/// Root ownership of the pool; kept by the io_uring backend.
pub(crate) struct BufferPoolRoot {
    shared: Rc<Shared>,
}

impl BufferPoolRoot {
    /// Allocate buffers, register the buf ring, and return the root.
    pub(crate) fn new(uring: &IoUring, num_bufs: u16, buffer_len: usize, flags: u16) -> io::Result<Self> {
        let size = u32::try_from(buffer_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer pool buffer length does not fit in u32",
            )
        })?;
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer pool buffer length must be non-zero",
            ));
        }

        let requested = num_bufs.max(1);
        if requested > MAX_BUFFER_RING_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("buffer pool size cannot exceed {MAX_BUFFER_RING_ENTRIES}"),
            ));
        }
        // The kernel requires a power-of-two ring size. The upper-bound check
        // above also guarantees this cannot overflow `u16`.
        let n = requested.next_power_of_two();
        let mut bufs: Vec<Option<NonNull<u8>>> = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let layout = std::alloc::Layout::from_size_align(size as usize, 1)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            // Safety: layout has non-zero size.
            let raw = unsafe { std::alloc::alloc(layout) };
            let Some(ptr) = NonNull::new(raw) else {
                // Free what we allocated so far.
                for slot in bufs.drain(..).flatten() {
                    unsafe {
                        std::alloc::dealloc(
                            slot.as_ptr(),
                            std::alloc::Layout::from_size_align_unchecked(size as usize, 1),
                        );
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "failed to allocate buffer pool memory",
                ));
            };
            bufs.push(Some(ptr));
        }

        let ring = unsafe { BufRing::register(uring, &bufs, size, flags) }.map_err(|err| {
            for slot in bufs.drain(..).flatten() {
                unsafe {
                    std::alloc::dealloc(
                        slot.as_ptr(),
                        std::alloc::Layout::from_size_align_unchecked(size as usize, 1),
                    );
                }
            }
            err
        })?;

        Ok(Self {
            shared: Rc::new(Shared {
                inner: UnsafeCell::new(Inner { ring, bufs, size }),
            }),
        })
    }

    pub(crate) fn handle(&self) -> BufferPool {
        BufferPool {
            shared: Rc::downgrade(&self.shared),
        }
    }

    /// Unregister the ring and free unleased buffers.
    ///
    /// # Safety
    ///
    /// This consumes the root. Outstanding [`PooledBuf`] values free their own
    /// memory on drop after their weak pool reference can no longer upgrade.
    pub(crate) unsafe fn release(self, uring: &IoUring) -> io::Result<()> {
        // Buffer handles and leases retain only Weak references, so the root
        // must be the sole strong owner during backend teardown.
        let shared = match Rc::try_unwrap(self.shared) {
            Ok(shared) => shared,
            Err(_) => {
                return Err(io::Error::other(
                    "buffer pool root was not uniquely owned during release",
                ));
            }
        };
        let Inner { ring, mut bufs, size } = shared.inner.into_inner();

        let unregister_result = match unsafe { ring.unregister(uring) } {
            BufRingRelease::Released(result) => result,
            BufRingRelease::StillRegistered(error) => {
                // Dropping the pointer table does not free its raw buffer
                // allocations. They intentionally remain alive because the
                // still-registered ring may reference them.
                return Err(error);
            }
        };
        for slot in bufs.drain(..).flatten() {
            unsafe {
                std::alloc::dealloc(
                    slot.as_ptr(),
                    std::alloc::Layout::from_size_align_unchecked(size as usize, 1),
                );
            }
        }
        unregister_result
    }
}

impl Shared {
    /// # Safety
    /// Caller must not re-enter into this pool via the same shared state.
    #[inline]
    unsafe fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(unsafe { &mut *self.inner.get() })
    }

    fn take(&self, buffer_id: u16) -> Option<NonNull<u8>> {
        unsafe { self.with(|inner| inner.bufs.get_mut(buffer_id as usize)?.take()) }
    }

    fn reset(&self, buffer_id: u16, ptr: NonNull<u8>) {
        unsafe {
            self.with(|inner| {
                if let Some(slot) = inner.bufs.get_mut(buffer_id as usize) {
                    *slot = Some(ptr);
                    inner.ring.reset_buffer(buffer_id, ptr, inner.size);
                } else {
                    // Pool already released; free the allocation ourselves.
                    std::alloc::dealloc(
                        ptr.as_ptr(),
                        std::alloc::Layout::from_size_align_unchecked(inner.size as usize, 1),
                    );
                }
            });
        }
    }

    fn size(&self) -> u32 {
        unsafe { self.with(|inner| inner.size) }
    }
}

impl BufferPool {
    fn shared(&self) -> io::Result<Rc<Shared>> {
        self.shared
            .upgrade()
            .ok_or_else(|| io::Error::other("the runtime buffer pool has been dropped"))
    }

    /// Take the buffer selected by the kernel for `buffer_id`.
    ///
    /// Returns `Ok(None)` if the slot is already leased or unknown.
    pub(crate) fn take(&self, buffer_id: u16) -> io::Result<Option<PooledBuf>> {
        let shared = self.shared()?;
        let Some(ptr) = shared.take(buffer_id) else {
            return Ok(None);
        };
        let cap = shared.size();
        Ok(Some(PooledBuf {
            shared: Rc::downgrade(&shared),
            ptr,
            offset: 0,
            len: 0,
            cap,
            full_cap: cap,
            buffer_id,
        }))
    }

    /// Return a previously taken buffer id to the pool without constructing a
    /// [`PooledBuf`] (same as take + drop).
    pub(crate) fn reset(&self, buffer_id: u16) -> io::Result<bool> {
        let shared = self.shared()?;
        let Some(ptr) = shared.take(buffer_id) else {
            return Ok(false);
        };
        shared.reset(buffer_id, ptr);
        Ok(true)
    }

    /// Recycle a kernel-selected buffer and flag ownership violations in debug builds.
    pub(crate) fn recycle_selected(&self, buffer_id: u16) {
        match self.reset(buffer_id) {
            Ok(true) => {}
            Ok(false) => debug_assert!(false, "selected buffer {buffer_id} was not available to recycle"),
            Err(error) => debug_assert!(false, "failed to recycle selected buffer {buffer_id}: {error}"),
        }
    }

    /// Buffer group id used with `IOSQE_BUFFER_SELECT`.
    #[inline]
    pub(crate) fn buffer_group(&self) -> io::Result<u16> {
        // Fail if the pool is gone so callers do not submit with a stale bgid.
        let _ = self.shared()?;
        Ok(BUFFER_GROUP_ID)
    }

    /// Byte length of each pool buffer.
    pub(crate) fn buffer_len(&self) -> io::Result<usize> {
        Ok(self.shared()?.size() as usize)
    }
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferPool")
            .field("alive", &self.shared.upgrade().is_some())
            .finish()
    }
}

impl PooledBuf {
    /// Restrict this lease to an initialized subrange of its allocation.
    ///
    /// This lets packed kernel results expose their payload without moving it.
    pub(crate) fn set_view(&mut self, range: std::ops::Range<usize>) -> io::Result<()> {
        if range.start > range.end || range.end > self.len as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "buffer view lies outside initialized data",
            ));
        }

        self.offset = range.start as u32;
        self.len = (range.end - range.start) as u32;
        self.cap = self.full_cap - self.offset;
        Ok(())
    }

    /// Explicitly return this lease to the pool.
    ///
    /// Equivalent to dropping the value; useful when an upper layer wants the
    /// slot recycled before the end of a large scope.
    #[inline]
    pub fn release(self) {
        drop(self);
    }

    /// Restrict the writable capacity of this lease (cannot grow past the
    /// underlying allocation).
    pub fn set_capacity(&mut self, cap: usize) {
        if cap == 0 {
            return;
        }
        let max_cap = self.full_cap - self.offset;
        self.cap = u32::try_from(cap).unwrap_or(u32::MAX).min(max_cap);
        self.len = self.len.min(self.cap);
    }

    /// Restrict capacity and return `self` (builder-style).
    pub fn with_capacity(mut self, cap: usize) -> Self {
        self.set_capacity(cap);
        self
    }
}

impl fmt::Debug for PooledBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PooledBuf")
            .field("buffer_id", &self.buffer_id)
            .field("offset", &self.offset)
            .field("len", &self.len)
            .field("cap", &self.cap)
            .finish()
    }
}

impl Deref for PooledBuf {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // Safety: `len` is only advanced after kernel fill or `set_len`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr().add(self.offset as usize), self.len as usize) }
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.offset as usize), self.len as usize) }
    }
}

impl IoBuf for PooledBuf {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl SetLen for PooledBuf {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.cap as usize);
        self.len = (len as u32).min(self.cap);
    }
}

impl IoBufMut for PooledBuf {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        // Safety: the view offset and capacity stay within the allocation.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.offset as usize).cast(), self.cap as usize) }
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.reset(self.buffer_id, self.ptr);
        } else {
            // Runtime/pool already torn down: free the allocation ourselves.
            unsafe {
                std::alloc::dealloc(
                    self.ptr.as_ptr(),
                    std::alloc::Layout::from_size_align_unchecked(self.full_cap as usize, 1),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::RuntimeBuilder;
    use crate::runtime::local::CURRENT_DRIVER;

    #[test]
    fn take_and_release_recycles_slot() {
        let mut rt = RuntimeBuilder::new()
            .buffer_pool_size(4)
            .buffer_pool_buffer_len(1024)
            .build()
            .expect("runtime");

        rt.block_on(async {
            let pool = CURRENT_DRIVER
                .with(|h| h.upgrade().expect("runtime driver").buffer_pool())
                .expect("buffer pool");

            assert_eq!(pool.buffer_group().unwrap(), BUFFER_GROUP_ID);
            assert_eq!(pool.buffer_len().unwrap(), 1024);

            let mut buf = pool.take(0).unwrap().expect("slot 0 available");
            assert_eq!(buf.buffer_id, 0);
            assert!(buf.as_init().is_empty());
            buf.as_uninit()[..4].copy_from_slice(&[
                MaybeUninit::new(1),
                MaybeUninit::new(2),
                MaybeUninit::new(3),
                MaybeUninit::new(4),
            ]);
            // Safety: the four bytes above were just initialized.
            unsafe { buf.set_len(4) };
            assert_eq!(&buf[..], &[1, 2, 3, 4]);

            // Slot is leased: second take fails.
            assert!(pool.take(0).unwrap().is_none());

            buf.release();
            let again = pool.take(0).unwrap().expect("slot recycled");
            assert_eq!(again.buffer_id, 0);
            again.release();
        });
    }
}
