use std::{io, mem::MaybeUninit};

use crate::buf::IoBuf;

/// Updates the initialized length of a buffer after an input operation.
pub trait SetLen {
    /// Sets the exact number of initialized bytes in the buffer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `len` does not exceed the buffer capacity
    /// and that every byte in `0..len` is initialized.
    unsafe fn set_len(&mut self, len: usize);
}

/// A trait for mutable buffers.
///
/// The `IoBufMut` trait is implemented by buffer types that can be passed to
/// mutable completion-based IO operations, like reading content from a file and
/// write to the buffer. This trait will take all space of a buffer into account,
/// including uninitialized bytes
pub trait IoBufMut: IoBuf + SetLen {
    /// Returns the full buffer, including initialized and uninitialized bytes.
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>];

    /// Ensures space for `additional` bytes after the initialized prefix.
    /// By default, this checks if the spare capacity is enough to fit in `len`-bytes.
    /// If it does, returns `Ok(())`, and otherwise returns an error
    ///
    /// Types that support dynamicresizing (like `Vec<u8>`)
    /// will override this method to actually reserve capacity.
    fn reserve(&mut self, additional: usize) -> io::Result<()> {
        let initialized = <Self as IoBuf>::as_init(self).len();
        let capacity = self.as_uninit().len();
        if additional <= capacity.saturating_sub(initialized) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "buffer does not support reserving additional capacity",
            ))
        }
    }
}

impl<B: SetLen + ?Sized> SetLen for Box<B> {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        unsafe { (**self).set_len(len) }
    }
}

impl<B: SetLen + ?Sized> SetLen for &'static mut B {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        unsafe { (**self).set_len(len) }
    }
}

impl<B: IoBufMut + ?Sized> IoBufMut for Box<B> {
    #[inline]
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        (**self).as_uninit()
    }

    #[inline]
    fn reserve(&mut self, additional: usize) -> io::Result<()> {
        (**self).reserve(additional)
    }
}

impl<B: IoBufMut + ?Sized> IoBufMut for &'static mut B {
    #[inline]
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        (**self).as_uninit()
    }

    #[inline]
    fn reserve(&mut self, additional: usize) -> io::Result<()> {
        (**self).reserve(additional)
    }
}

impl SetLen for Vec<u8> {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity(), "initialized length exceeds buffer capacity");
        unsafe { Vec::set_len(self, len) }
    }
}

impl IoBufMut for Vec<u8> {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        let ptr = self.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let capacity = self.capacity();
        // Safety: a Vec allocation is valid for `capacity` elements and
        // MaybeUninit permits access to both initialized and spare bytes.
        unsafe { std::slice::from_raw_parts_mut(ptr, capacity) }
    }

    fn reserve(&mut self, additional: usize) -> io::Result<()> {
        self.try_reserve(additional)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))
    }
}

impl SetLen for [u8] {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.len(), "initialized length exceeds buffer capacity");
    }
}

impl IoBufMut for [u8] {
    #[inline]
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        // Safety: MaybeUninit<u8> has the same layout as u8 and all bytes in a
        // byte slice are already initialized.
        unsafe { &mut *(self as *mut [u8] as *mut [MaybeUninit<u8>]) }
    }
}

impl<const N: usize> SetLen for [u8; N] {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= N, "initialized length exceeds buffer capacity");
    }
}

impl<const N: usize> IoBufMut for [u8; N] {
    #[inline]
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        // Safety: MaybeUninit<u8> has the same layout as u8 and the array is
        // valid for exactly N bytes.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast(), N) }
    }
}

#[cfg(feature = "bytes")]
#[cfg_attr(docsrs, doc(cfg(feature = "bytes")))]
impl SetLen for bytes::BytesMut {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity(), "initialized length exceeds buffer capacity");
        // Safety: upheld by SetLen's caller contract and checked capacity.
        unsafe { bytes::BytesMut::set_len(self, len) };
    }
}

#[cfg(feature = "bytes")]
#[cfg_attr(docsrs, doc(cfg(feature = "bytes")))]
impl IoBufMut for bytes::BytesMut {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        let ptr = self.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let capacity = self.capacity();
        // Safety: a BytesMut allocation is valid for `capacity` elements and
        // MaybeUninit permits access to both initialized and spare bytes.
        unsafe { std::slice::from_raw_parts_mut(ptr, capacity) }
    }

    fn reserve(&mut self, additional: usize) -> io::Result<()> {
        // BytesMut::reserve grows in place and does not return a Result.
        bytes::BytesMut::reserve(self, additional);
        Ok(())
    }
}

#[cfg(feature = "memmap2")]
#[cfg_attr(docsrs, doc(cfg(feature = "memmap2")))]
impl SetLen for memmap2::MmapMut {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.len(), "initialized length exceeds buffer capacity");
    }
}

#[cfg(feature = "memmap2")]
#[cfg_attr(docsrs, doc(cfg(feature = "memmap2")))]
impl IoBufMut for memmap2::MmapMut {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        let bytes: &mut [u8] = self;
        // Safety: `MmapMut` derefs to a byte slice whose backing pages are
        // always fully initialized, and `MaybeUninit<u8>` has the same layout
        // as `u8`.
        unsafe { &mut *(bytes as *mut [u8] as *mut [MaybeUninit<u8>]) }
    }
}

#[cfg(all(test, feature = "bytes"))]
mod bytes_tests {
    use super::{IoBufMut, SetLen};
    use crate::buf::{IoBuf, IoBufExt, IoBufMutExt};

    #[test]
    fn bytes_mut_as_uninit_covers_full_capacity() {
        let mut buf = bytes::BytesMut::with_capacity(16);
        buf.extend_from_slice(b"abcd");
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.as_uninit().len(), 16);
    }

    #[test]
    fn bytes_mut_reserve_grows_spare_capacity() {
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(b"ab");
        IoBufMut::reserve(&mut buf, 32).unwrap();
        assert!(buf.capacity().saturating_sub(buf.len()) >= 32);
    }

    #[test]
    fn slice_spare_tail_set_len_preserves_bytes_mut_prefix() {
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(b"abcd");
        IoBufMut::reserve(&mut buf, 8).unwrap();

        let start = buf.len();
        let mut slice = IoBufExt::slice(buf, start..);
        assert_eq!(slice.as_init(), b"");
        assert!(slice.buf_capacity() >= 3);

        // Simulate a completion filling the spare tail without touching the prefix.
        {
            let uninit = slice.as_uninit();
            assert!(uninit.len() >= 3);
            for (index, byte) in b"efg".iter().enumerate() {
                uninit[index].write(*byte);
            }
        }
        unsafe { SetLen::set_len(&mut slice, 3) };

        let buf = slice.into_inner();
        assert_eq!(buf.as_init(), b"abcdefg");
        assert_eq!(&buf[..4], b"abcd");
    }
}

#[cfg(all(test, feature = "memmap2"))]
mod memmap2_tests {
    use super::{IoBufMut, SetLen};
    use crate::buf::{IoBuf, IoBufExt, IoBufMutExt};

    #[test]
    fn mmap_mut_as_uninit_covers_full_mapping() {
        let mut buf = memmap2::MmapMut::map_anon(16).unwrap();
        buf[..4].copy_from_slice(b"abcd");
        assert_eq!(buf.as_init().len(), 16);
        assert_eq!(buf.as_uninit().len(), 16);
    }

    #[test]
    fn mmap_mut_uninit_bytes_can_be_written() {
        let mut buf = memmap2::MmapMut::map_anon(8).unwrap();
        let uninit = buf.as_uninit();
        uninit[0].write(b'a');
        uninit[1].write(b'b');
        assert_eq!(unsafe { uninit[0].assume_init() }, b'a');
        assert_eq!(unsafe { uninit[1].assume_init() }, b'b');
    }

    #[test]
    fn mmap_mut_set_len_within_mapping_is_noop() {
        let mut buf = memmap2::MmapMut::map_anon(8).unwrap();
        buf[..4].copy_from_slice(b"abcd");
        unsafe { SetLen::set_len(&mut buf, 8) };
        assert_eq!(buf.as_init(), &b"abcd\0\0\0\0"[..]);
    }

    #[test]
    fn mmap_mut_reserve_is_unsupported() {
        let mut buf = memmap2::MmapMut::map_anon(8).unwrap();
        assert!(IoBufMut::reserve(&mut buf, 1).is_err());
    }

    #[test]
    fn mmap_mut_slice_tail_can_be_read_into() {
        let mut buf = memmap2::MmapMut::map_anon(8).unwrap();
        buf[..4].copy_from_slice(b"abcd");

        // An MmapMut is fully initialized, so an open-ended tail view covers
        // the remaining mapping bytes (offset 4..8) right away.
        let mut slice = IoBufExt::slice(buf, 4..);
        assert_eq!(slice.as_init().len(), 4);
        assert!(slice.buf_capacity() >= 3);

        {
            let uninit = slice.as_uninit();
            assert!(uninit.len() >= 3);
            for (index, byte) in b"efg".iter().enumerate() {
                uninit[index].write(*byte);
            }
        }
        assert_eq!(&slice[..3], b"efg");
        unsafe { SetLen::set_len(&mut slice, 3) };

        let buf = slice.into_inner();
        assert_eq!(buf.as_init(), &b"abcdefg\0"[..]);
        assert_eq!(&buf[..4], b"abcd");
    }
}
