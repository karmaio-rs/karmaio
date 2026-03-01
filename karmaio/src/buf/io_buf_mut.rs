use crate::buf::IoBuf;

// An `io_uring` compatible buffer.
//
// The `IoBuf` trait is implemented by buffer types that can be passed to io_uring operations.
// Users will not need to use this trait directly.
//
// Buffers passed to `io-uring` operations must reference a stable memory region.
// While the runtime holds ownership to a buffer, the pointer returned
// by `stable_mut_ptr` must remain valid even if the `IoBufMut` value is moved.
pub unsafe trait IoBufMut: IoBuf {
    // Returns a raw mutable pointer to the vector’s buffer.
    //
    // This method is to be used by the runtime and it is not expected for users to call it directly.
    //
    // The implementation must ensure that, while the runtime owns the value,
    // the pointer returned by `stable_write_ptr` **does not** change.
    fn stable_write_ptr(&mut self) -> *mut u8;

    // Updates the number of initialized bytes.
    //
    // If the specified `pos` is greater than the value returned by
    // [`IoBuf::bytes_init`], it becomes the new water mark as returned by
    // `IoBuf::bytes_init`.
    //
    // # Safety
    //
    // The caller must ensure that all bytes starting at `stable_write_ptr()`
    // up to `pos` are initialized and owned by the buffer.
    fn set_init(&mut self, pos: usize);
}

unsafe impl IoBufMut for Vec<u8> {
    #[inline]
    fn stable_write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    fn set_init(&mut self, init_len: usize) {
        if self.len() < init_len {
            unsafe { self.set_len(init_len) };
        }
    }
}

unsafe impl IoBufMut for Box<[u8]> {
    #[inline]
    fn stable_write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    fn set_init(&mut self, _pos: usize) {
        // Box<[u8]> is always fully initialized – no-op
    }
}

unsafe impl<const N: usize> IoBufMut for [u8; N] {
    #[inline]
    fn stable_write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    fn set_init(&mut self, _pos: usize) {
        // Fixed-size array is always fully initialized
    }
}
