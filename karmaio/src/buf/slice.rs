use std::{
    cmp,
    ops::{self, Bound},
    ptr,
};

use crate::buf::{BoundedIoBuf, BoundedIoBufMut, IoBuf, IoBufMut};

// An owned view into a contiguous sequence of bytes.
//
// This is similar to Rust slices (`&buf[..]`) but owns the underlying buffer.
// This type is useful for performing io-uring read and write operations using a subset of a buffer.
//
// `Slice<T>` always wraps the original base `IoBuf` (never `Slice<Slice<…>>`).
// Chaining `.slice().slice().slice()` stays flat → perfect monomorphisation.
pub struct Slice<T> {
    buf: T,
    start: usize,
    end: usize,
}

impl<T> Slice<T> {
    pub(crate) fn new(buf: T, start: usize, end: usize) -> Slice<T> {
        Slice { buf, start, end }
    }

    // Offset in the underlying buffer at which this slice starts.
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }

    // Ofset in the underlying buffer at which this slice ends.
    #[inline]
    pub fn end(&self) -> usize {
        self.end
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    // Gets a reference to the underlying buffer.
    #[inline]
    pub fn get_ref(&self) -> &T {
        &self.buf
    }

    // Gets a mutable reference to the underlying buffer.
    //
    // This method escapes the slice's view.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.buf
    }

    // Unwraps this `Slice`, returning the underlying buffer.
    #[inline]
    pub fn into_inner(self) -> T {
        self.buf
    }
}

impl<T: IoBuf> ops::Deref for Slice<T> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // Safety: the `IoBuf` trait is marked as unsafe and is expected to be implemented correctly.
        let buf_bytes = unsafe { std::slice::from_raw_parts(self.buf.stable_read_ptr(), self.buf.bytes_init()) };
        let end = cmp::min(self.end, buf_bytes.len());
        &buf_bytes[self.start..end]
    }
}

impl<T: IoBufMut> ops::DerefMut for Slice<T> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // Safety: the `IoBufMut` trait is marked as unsafe and is expected to be implemented correct.
        let buf_bytes = unsafe { std::slice::from_raw_parts_mut(self.buf.stable_write_ptr(), self.buf.bytes_init()) };
        let end = cmp::min(self.end, buf_bytes.len());
        &mut buf_bytes[self.start..end]
    }
}

impl<T: IoBuf> BoundedIoBuf for Slice<T> {
    type Buf = T;
    type Bounds = ops::Range<usize>;

    fn slice(self, range: impl ops::RangeBounds<usize>) -> Slice<T> {
        let start = match range.start_bound() {
            Bound::Included(&n) => self.start.checked_add(n).expect("out of range"),
            Bound::Excluded(&n) => self
                .start
                .checked_add(n)
                .and_then(|x| x.checked_add(1))
                .expect("out of range"),
            Bound::Unbounded => self.start,
        };

        assert!(start <= self.end);

        let end = match range.end_bound() {
            Bound::Included(&n) => self
                .start
                .checked_add(n)
                .and_then(|x| x.checked_add(1))
                .expect("out of range"),
            Bound::Excluded(&n) => self.start.checked_add(n).expect("out of range"),
            Bound::Unbounded => self.end,
        };

        assert!(end <= self.end);
        assert!(start <= self.buf.bytes_init());

        Slice::new(self.buf, start, end)
    }

    #[inline]
    fn slice_full(self) -> Slice<T> {
        self
    }

    #[inline]
    fn get_buf(&self) -> &T {
        &self.buf
    }

    #[inline]
    fn bounds(&self) -> Self::Bounds {
        self.start..self.end
    }

    fn from_buf_bounds(buf: T, bounds: Self::Bounds) -> Self {
        assert!(bounds.start <= buf.bytes_init());
        assert!(bounds.end <= buf.bytes_total());
        Slice::new(buf, bounds.start, bounds.end)
    }

    fn stable_read_ptr(&self) -> *const u8 {
        // Safety: the `IoBuf` trait is marked as unsafe and is expected to be implemented correctly.
        let buf_bytes = unsafe { std::slice::from_raw_parts(self.buf.stable_read_ptr(), self.buf.bytes_init()) };
        buf_bytes[self.start..].as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        ops::Deref::deref(self).len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.end - self.start
    }
}

impl<T: IoBufMut> BoundedIoBufMut for Slice<T> {
    type BufMut = T;

    fn put_slice(&mut self, src: &[u8]) {
        assert!(self.bytes_total() >= src.len());

        // Safety:
        // - dst pointer validity is ensured by stable_write_ptr.
        // - The length is checked to not exceed the view's total capacity
        // - src (immutable) and dst (mutable) cannot point to overlapping memory.
        // - After copying the amount of bytes given by the slice, it's safe to mark them as initialized in the buffer.
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.stable_write_ptr(), src.len());
            self.set_init(src.len());
        }
    }

    fn stable_write_ptr(&mut self) -> *mut u8 {
        // Safety: the `IoBufMut` trait is marked as unsafe and is expected to be implemented correct.
        let buf_bytes = unsafe { std::slice::from_raw_parts_mut(self.buf.stable_write_ptr(), self.buf.bytes_init()) };
        buf_bytes[self.start..].as_mut_ptr()
    }

    #[inline]
    unsafe fn set_init(&mut self, pos: usize) {
        self.buf.set_init(self.start + pos);
    }
}
