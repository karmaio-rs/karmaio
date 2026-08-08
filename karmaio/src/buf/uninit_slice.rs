use std::mem::MaybeUninit;

use crate::buf::{IntoInner, IoBuf, IoBufMut, SetLen, Slice};

/// An owned view that exposes only a buffer's uninitialized tail.
///
/// Create this adapter with [`crate::buf::IoBufMutExt::uninit`] immediately
/// before an input operation, then use [`IntoInner`] to recover the full
/// buffer. The initialized bytes already present in the buffer are preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UninitSlice<T>(Slice<T>);

impl<T: IoBufMut> UninitSlice<T> {
    pub(crate) fn new(buffer: T) -> Self {
        let start = buffer.as_init().len();
        // Safety: start is exactly the underlying initialized length.
        Self(unsafe { Slice::new_unchecked(buffer, start, None) })
    }
}

impl<T> UninitSlice<T> {
    /// Returns the offset at which the uninitialized tail begins.
    #[inline]
    pub fn start(&self) -> usize {
        self.0.start()
    }

    /// Returns a reference to the underlying buffer.
    #[inline]
    pub fn get_ref(&self) -> &T {
        self.0.get_ref()
    }

    /// Returns a mutable reference to the underlying buffer.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

impl<T: IoBuf> IoBuf for UninitSlice<T> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self.0.as_init()
    }
}

impl<T: IoBufMut> IoBufMut for UninitSlice<T> {
    #[inline]
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        self.0.as_uninit()
    }

    #[inline]
    fn reserve(&mut self, additional: usize) -> std::io::Result<()> {
        self.0.reserve(additional)
    }
}

impl<T: SetLen> SetLen for UninitSlice<T> {
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        // Safety: the caller initialized `len` bytes in this view, which maps
        // directly to the underlying buffer after the original prefix.
        unsafe { self.0.set_len(len) }
    }
}

impl<T> IntoInner for UninitSlice<T> {
    type Inner = T;

    #[inline]
    fn into_inner(self) -> Self::Inner {
        self.0.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::IoBufMutExt;

    #[test]
    fn exposes_only_the_uninitialized_tail() {
        let mut buffer = Vec::with_capacity(8);
        buffer.extend_from_slice(b"abc");
        let mut tail = buffer.uninit();

        assert_eq!(tail.start(), 3);
        assert!(tail.as_init().is_empty());
        assert!(tail.as_uninit().len() >= 5);
        tail.as_uninit()[..2].copy_from_slice(&[MaybeUninit::new(b'd'), MaybeUninit::new(b'e')]);

        // Safety: the first two bytes exposed by the tail were initialized.
        unsafe { tail.set_len(2) };
        assert_eq!(tail.as_init(), b"de");
        assert_eq!(tail.into_inner(), b"abcde");
    }
}
