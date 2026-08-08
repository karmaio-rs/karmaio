use std::mem::MaybeUninit;

use crate::buf::{IntoInner, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut, SetLen};

/// An owned scalar-buffer cursor over a vectored buffer collection.
///
/// This adapter allows scalar I/O implementations to operate directly on one
/// component without allocating or copying its contents.
pub struct VectoredBufIterator<B> {
    buf: B,
    total_filled: usize,
    index: usize,
    count: usize,
    filled: usize,
}

impl<B> VectoredBufIterator<B> {
    /// Advances to the next component.
    ///
    /// Returns the original collection when the cursor reaches the end.
    pub fn next(mut self) -> Result<Self, B> {
        self.index += 1;
        if self.index < self.count {
            self.total_filled = self
                .total_filled
                .checked_add(self.filled)
                .expect("vectored initialized length overflow");
            self.filled = 0;
            Ok(self)
        } else {
            Err(self.buf)
        }
    }

    /// Returns the current component index.
    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }
}

impl<B: IoVectoredBuf> VectoredBufIterator<B> {
    pub(crate) fn new(buf: B) -> Result<Self, B> {
        let count = buf.iter_slice().count();
        if count == 0 {
            Err(buf)
        } else {
            Ok(Self {
                buf,
                total_filled: 0,
                index: 0,
                count,
                filled: 0,
            })
        }
    }
}

impl<B> IntoInner for VectoredBufIterator<B> {
    type Inner = B;

    #[inline]
    fn into_inner(self) -> Self::Inner {
        self.buf
    }
}

impl<B: IoVectoredBuf> IoBuf for VectoredBufIterator<B> {
    fn as_init(&self) -> &[u8] {
        let current = self
            .buf
            .iter_slice()
            .nth(self.index)
            .expect("vectored cursor index exceeds component count");
        &current[self.filled..]
    }
}

impl<B: IoVectoredBuf + SetLen> SetLen for VectoredBufIterator<B> {
    unsafe fn set_len(&mut self, len: usize) {
        self.filled = len;
        let total = self
            .total_filled
            .checked_add(len)
            .expect("vectored initialized length overflow");
        // Safety: the caller initialized `len` bytes in the current component;
        // earlier components account for `total_filled` initialized bytes.
        unsafe { self.buf.set_len(total) }
    }
}

impl<B: IoVectoredBufMut> IoBufMut for VectoredBufIterator<B> {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        self.buf
            .iter_uninit_slice()
            .nth(self.index)
            .expect("vectored cursor index exceeds component count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_iterator_exposes_components_without_copying() {
        let bufs = [Vec::new(), b"abc".to_vec()];
        let iter = bufs.owned_iter().expect("collection is not empty");
        assert_eq!(iter.index(), 0);
        assert!(iter.as_init().is_empty());

        let iter = iter.next().expect("second component exists");
        assert_eq!(iter.index(), 1);
        assert_eq!(iter.as_init(), b"abc");
        assert_eq!(iter.into_inner(), [Vec::new(), b"abc".to_vec()]);
    }

    #[test]
    fn owned_mutable_iterator_updates_original_component_lengths() {
        let bufs = [Vec::with_capacity(0), Vec::with_capacity(4)];
        let iter = bufs.owned_iter().expect("collection is not empty");
        let mut iter = iter.next().expect("second component exists");
        iter.as_uninit()[..2].fill(MaybeUninit::new(7));

        // Safety: two bytes in the current component were initialized above.
        unsafe { iter.set_len(2) };
        let bufs = iter.into_inner();
        assert!(bufs[0].is_empty());
        assert_eq!(bufs[1], [7, 7]);
    }
}
