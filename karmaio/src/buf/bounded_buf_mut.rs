use std::ptr;

use crate::buf::{BoundedIoBuf, IoBufMut};

// A slice from an owned [`IoBufMut`] buffer.
//
// This trait provides a generic way to use mutable buffers and `Slice` views
// into such buffers with `io-uring` operations.
pub trait BoundedIoBufMut: BoundedIoBuf<Buf = Self::BufMut> {
    // The type of the underlying buffer.
    type BufMut: IoBufMut;

    // Copies the given byte slice into the buffer, starting at this view's offset.
    //
    // # Panics
    //
    // If the slice's length exceeds the destination's total capacity, this method panics.
    fn put_slice(&mut self, src: &[u8]);

    // Like [`IoBufMut::stable_write_ptr`],
    // but possibly offset to the view's starting position.
    fn stable_write_ptr(&mut self) -> *mut u8;

    // Like [`IoBufMut::set_init`],
    // but the position is possibly offset to the view's starting position.
    //
    // # Safety
    //
    // The caller must ensure that all bytes starting at `stable_write_ptr()`
    // up to `pos` are initialized and owned by the buffer.
    unsafe fn set_init(&mut self, pos: usize);
}

impl<T: IoBufMut> BoundedIoBufMut for T {
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

    #[inline]
    fn stable_write_ptr(&mut self) -> *mut u8 {
        IoBufMut::stable_write_ptr(self)
    }

    #[inline]
    unsafe fn set_init(&mut self, pos: usize) {
        IoBufMut::set_init(self, pos)
    }
}
