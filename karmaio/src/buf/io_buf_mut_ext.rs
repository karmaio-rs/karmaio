use std::{mem::MaybeUninit, ops::RangeBounds, ptr};

use crate::buf::{IoBuf, IoBufMut, UninitSlice};

/// Convenience methods for mutable completion buffers.
pub trait IoBufMutExt: IoBufMut {
    /// Restricts this owned buffer to its currently uninitialized tail.
    ///
    /// The returned view is intended for one input operation. Recover the
    /// original buffer through [`crate::buf::IntoInner`] after completion.
    #[inline]
    fn uninit(self) -> UninitSlice<Self>
    where
        Self: Sized,
    {
        UninitSlice::new(self)
    }

    /// Returns the total writable capacity, including initialized bytes.
    #[inline]
    fn buf_capacity(&mut self) -> usize {
        self.as_uninit().len()
    }

    /// Returns a raw mutable pointer to the full writable buffer.
    #[inline]
    fn buf_mut_ptr(&mut self) -> *mut MaybeUninit<u8> {
        self.as_uninit().as_mut_ptr()
    }

    /// Get the mutable slice of initialized bytes.
    /// The content is the same as [`IoBuf::as_init`], but mutable.
    fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = <Self as IoBuf>::as_init(self).len();
        let ptr = self.as_uninit().as_mut_ptr().cast::<u8>();
        // Safety: IoBuf guarantees that 0..len is initialized, and the mutable
        // borrow prevents aliases for the returned lifetime.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Initialize all bytes in the buffer and return them.
    ///
    /// Already initialized bytes are preserved.
    /// Uninitialized tail bytes are zero-initialized.
    fn ensure_init(&mut self) -> &mut [u8] {
        let initialized = <Self as IoBuf>::as_init(self).len();
        let buf = self.as_uninit();
        buf[initialized..].fill(MaybeUninit::new(0));
        // Safety: the initialized prefix was already valid and the tail was
        // initialized immediately above. MaybeUninit<u8> has the same layout
        // as u8, and the returned slice remains bound to this mutable borrow.
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len()) }
    }

    /// Clears the initialized region without releasing capacity.
    fn clear(&mut self) {
        // Safety: a zero-length initialized prefix is always valid.
        unsafe { self.set_len(0) }
    }

    /// Shrinks the initialized region without releasing capacity.
    fn truncate(&mut self, len: usize) {
        assert!(
            len <= <Self as IoBuf>::as_init(self).len(),
            "truncate exceeds initialized length"
        );
        // Safety: the retained prefix was already initialized.
        unsafe { self.set_len(len) }
    }

    /// Extend the buffer by appending bytes from `src`.
    ///
    /// The buffer will reserve additional capacity if necessary.
    fn extend_from_slice(&mut self, src: &[u8]) {
        let initialized = <Self as IoBuf>::as_init(self).len();
        self.reserve(src.len()).expect("failed to reserve buffer capacity");
        let capacity = self.as_uninit().len();
        let end = initialized.checked_add(src.len()).expect("buffer length overflow");
        assert!(end <= capacity, "reserved buffer capacity is too small");

        // Safety: the destination range lies inside the uniquely borrowed
        // buffer and cannot overlap the external source slice.
        unsafe {
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.as_uninit().as_mut_ptr().cast::<u8>().add(initialized),
                src.len(),
            );
            self.set_len(end);
        }
    }

    /// Copy a range of bytes within the buffer to another location in the same buffer.
    ///
    /// This will copy within the full initialized and uninitialized allocation.
    ///
    /// # Panics
    ///
    /// This function will panic if either range exceeds the end of the slice,
    /// or if the end of `src` is before the start.
    fn copy_within(&mut self, src: impl RangeBounds<usize>, dest: usize) {
        self.as_uninit().copy_within(src, dest);
    }

    /// Copies a slice into the beginning of this buffer.
    fn put_slice(&mut self, src: &[u8]) {
        assert!(src.len() <= self.buf_capacity(), "source exceeds buffer capacity");
        // Safety: the destination has enough capacity, is uniquely borrowed,
        // and every byte marked initialized is written by the copy.
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.buf_mut_ptr().cast::<u8>(), src.len());
            self.set_len(src.len());
        }
    }
}

impl<B: IoBufMut + ?Sized> IoBufMutExt for B {}
