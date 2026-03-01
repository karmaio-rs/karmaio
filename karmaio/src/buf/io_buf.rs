// An `io_uring` compatible buffer.
//
// The `IoBuf` trait is implemented by buffer types that can be passed to
// io_uring operations. Users will not need to use this trait directly.
//
// # Safety
//
// Buffers passed to `io-uring` operations must reference a stable memory region.
// While the runtime holds ownership to a buffer, the pointer returned
// by `stable_read_ptr` must remain valid even if the `IoBuf` value is moved.
pub unsafe trait IoBuf: Unpin + 'static {
    // Returns a raw pointer to the vector’s buffer.
    //
    // This method is to be used by the runtime and we do not expect users to call it directly.
    //
    // The implementation must ensure that, while the runtime owns the value,
    // the pointer returned by `stable_read_ptr` **does not**  change.
    fn stable_read_ptr(&self) -> *const u8;

    // Number of initialized bytes.
    //
    // This method is to be used by the runtime and it is not
    // expected for users to call it directly.
    //
    // For `Vec`, this is identical to `len()`.
    fn bytes_init(&self) -> usize;

    // Total size of the buffer, including uninitialized memory, if any.
    //
    // This method is to be used by the runtime and it is not
    // expected for users to call it directly.
    //
    // For `Vec`, this is identical to `capacity()`.
    fn bytes_total(&self) -> usize;
}

unsafe impl IoBuf for Vec<u8> {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        self.len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.capacity()
    }
}

unsafe impl IoBuf for &'static [u8] {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        self.len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.bytes_init()
    }
}

unsafe impl IoBuf for Box<[u8]> {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        self.len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.bytes_init()
    }
}

unsafe impl<const N: usize> IoBuf for [u8; N] {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }
    #[inline]
    fn bytes_init(&self) -> usize {
        N
    }
    #[inline]
    fn bytes_total(&self) -> usize {
        N
    }
}

unsafe impl IoBuf for &'static str {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        self.len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.bytes_init()
    }
}

unsafe impl IoBuf for String {
    #[inline]
    fn stable_read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn bytes_init(&self) -> usize {
        self.len()
    }

    #[inline]
    fn bytes_total(&self) -> usize {
        self.bytes_init()
    }
}
