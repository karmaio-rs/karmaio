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
