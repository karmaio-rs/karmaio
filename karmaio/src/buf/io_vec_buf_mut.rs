use std::mem::MaybeUninit;

use crate::buf::{IoBufMut, IoVectoredBuf, SetLen, VectoredSlice};

/// An owned collection of mutable completion buffers.
///
/// Implementations must return the same slices in the same order on each call
/// while an operation owns the collection.
///
/// The iterator must be idemptotent and always yield the same slices in the exact same orders,
/// i.e., [`Iterator::enumerate`] will mark the same buffer with same index.
pub trait IoVectoredBufMut: IoVectoredBuf + SetLen {
    /// Iterates over the full capacity of each component buffer.
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]>;

    /// Returns the total capacity of all component buffers.
    fn total_capacity(&mut self) -> usize {
        self.iter_uninit_slice().map(|buf| buf.len()).sum()
    }

    /// Returns an owned view skipping `start` bytes of total capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use karmaio::buf::{IoVectoredBufMut, VectoredSlice};
    ///
    /// let bufs = [Vec::with_capacity(3), Vec::with_capacity(4)];
    /// let mut view: VectoredSlice<_> = IoVectoredBufMut::slice_mut(bufs, 2);
    ///
    /// assert_eq!(
    ///     view.iter_uninit_slice().map(|buf| buf.len()).collect::<Vec<_>>(),
    ///     [1, 4]
    /// );
    /// ```
    fn slice_mut(mut self, start: usize) -> VectoredSlice<Self>
    where
        Self: Sized,
    {
        assert!(
            start <= self.total_capacity(),
            "vectored slice starts beyond total capacity"
        );
        let mut remaining = start;
        let mut index = 0;
        for buf in self.iter_uninit_slice() {
            if remaining < buf.len() {
                break;
            }
            remaining -= buf.len();
            index += 1;
        }
        VectoredSlice::from_capacity_mut(self, start, index, remaining)
    }
}

impl<B: IoBufMut> IoVectoredBufMut for [B] {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        self.iter_mut().map(IoBufMut::as_uninit)
    }
}

impl<V: IoVectoredBufMut + ?Sized> IoVectoredBufMut for Box<V> {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        (**self).iter_uninit_slice()
    }
}

impl<V: IoVectoredBufMut + ?Sized> IoVectoredBufMut for &'static mut V {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        (**self).iter_uninit_slice()
    }
}

impl<B: IoBufMut, const N: usize> IoVectoredBufMut for [B; N] {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        self.iter_mut().map(IoBufMut::as_uninit)
    }
}

impl<B: IoBufMut> IoVectoredBufMut for Vec<B> {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        self.iter_mut().map(IoBufMut::as_uninit)
    }
}

impl IoVectoredBufMut for () {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        std::iter::empty()
    }
}

impl<B: IoBufMut, Rest: IoVectoredBufMut> IoVectoredBufMut for (B, Rest) {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        std::iter::once(self.0.as_uninit()).chain(self.1.iter_uninit_slice())
    }
}

impl<B: IoBufMut> IoVectoredBufMut for (B,) {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        std::iter::once(self.0.as_uninit())
    }
}

impl<B: IoBufMut> SetLen for [B] {
    unsafe fn set_len(&mut self, len: usize) {
        let mut remaining = len;
        for buf in self {
            let capacity = buf.as_uninit().len();
            let initialized = remaining.min(capacity);
            // Safety: initialized never exceeds this component's capacity and
            // the caller initialized the aggregate prefix.
            unsafe { buf.set_len(initialized) };
            remaining -= initialized;
        }
        assert_eq!(remaining, 0, "initialized length exceeds vectored capacity");
    }
}

impl<B: IoBufMut, const N: usize> SetLen for [B; N] {
    unsafe fn set_len(&mut self, len: usize) {
        // Safety: forwarded unchanged to the slice implementation.
        unsafe { self.as_mut_slice().set_len(len) }
    }
}

impl<B: IoBufMut> SetLen for Vec<B> {
    unsafe fn set_len(&mut self, len: usize) {
        // Safety: forwarded unchanged to the component-slice implementation.
        unsafe { self.as_mut_slice().set_len(len) }
    }
}

impl SetLen for () {
    unsafe fn set_len(&mut self, len: usize) {
        assert_eq!(len, 0, "initialized length exceeds empty vectored buffer");
    }
}

impl<B: IoBufMut, Rest: IoVectoredBufMut> SetLen for (B, Rest) {
    unsafe fn set_len(&mut self, len: usize) {
        let capacity = self.0.as_uninit().len();
        let initialized = len.min(capacity);
        // Safety: the aggregate prefix initializes this component prefix.
        unsafe { self.0.set_len(initialized) };
        // Safety: the remaining aggregate prefix belongs to the tail.
        unsafe { self.1.set_len(len - initialized) };
    }
}

impl<B: IoBufMut> SetLen for (B,) {
    unsafe fn set_len(&mut self, len: usize) {
        // Safety: forwarded unchanged to the only component.
        unsafe { self.0.set_len(len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_len_is_distributed_across_components() {
        let mut bufs = [Vec::with_capacity(3), Vec::with_capacity(4)];
        bufs[0].as_uninit().fill(MaybeUninit::new(1));
        bufs[1].as_uninit().fill(MaybeUninit::new(2));

        // Safety: both full capacities were initialized above.
        unsafe { SetLen::set_len(&mut bufs, 5) };

        assert_eq!(bufs[0], [1, 1, 1]);
        assert_eq!(bufs[1], [2, 2]);
    }
}
