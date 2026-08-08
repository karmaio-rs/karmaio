use crate::buf::{IoBuf, VectoredBufIterator, VectoredSlice};

/// An owned collection of immutable completion buffers.
///
/// Implementations must return the same slices in the same order on each call
/// while an operation owns the collection.
///
/// The iterator must be idemptotent and always yield the same slices in the exact same orders,
/// i.e., [`Iterator::enumerate`] will mark the same buffer with same index.
pub trait IoVectoredBuf: 'static {
    /// Iterates over the initialized bytes of each component buffer.
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]>;

    /// Returns the total number of initialized bytes.
    fn total_len(&self) -> usize {
        self.iter_slice().map(<[u8]>::len).sum()
    }

    /// Wraps this collection in an owned scalar-buffer cursor.
    ///
    /// Returns the original collection when it contains no component buffers.
    fn owned_iter(self) -> Result<VectoredBufIterator<Self>, Self>
    where
        Self: Sized,
    {
        VectoredBufIterator::new(self)
    }

    /// Returns an owned view skipping `start` initialized bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use karmaio::buf::{IoVectoredBuf, VectoredSlice};
    ///
    /// let bufs = [Vec::from(b"abc"), Vec::from(b"def")];
    /// let view: VectoredSlice<_> = IoVectoredBuf::slice(bufs, 4);
    ///
    /// assert_eq!(view.iter_slice().collect::<Vec<_>>(), [b"ef".as_slice()]);
    /// assert_eq!(view.into_inner(), [b"abc".to_vec(), b"def".to_vec()]);
    /// ```
    fn slice(self, start: usize) -> VectoredSlice<Self>
    where
        Self: Sized,
    {
        VectoredSlice::from_initialized(self, start)
    }
}

impl<B: IoBuf> IoVectoredBuf for [B] {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(IoBuf::as_init)
    }
}

impl<V: IoVectoredBuf + ?Sized> IoVectoredBuf for Box<V> {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        (**self).iter_slice()
    }
}

impl<V: IoVectoredBuf + ?Sized> IoVectoredBuf for &'static V {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        (**self).iter_slice()
    }
}

impl<V: IoVectoredBuf + ?Sized> IoVectoredBuf for &'static mut V {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        (**self).iter_slice()
    }
}

impl<B: IoBuf, const N: usize> IoVectoredBuf for [B; N] {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(IoBuf::as_init)
    }
}

impl<B: IoBuf> IoVectoredBuf for Vec<B> {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(IoBuf::as_init)
    }
}

impl IoVectoredBuf for () {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::empty()
    }
}

impl<B: IoBuf, REST: IoVectoredBuf> IoVectoredBuf for (B, REST) {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.0.as_init()).chain(self.1.iter_slice())
    }
}

impl<B: IoBuf> IoVectoredBuf for (B,) {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.0.as_init())
    }
}
