use std::{
    mem::MaybeUninit,
    ops::{Bound, Deref, DerefMut, RangeBounds},
};

use crate::buf::{IntoInner, IoBuf, IoBufExt, IoBufMut, IoBufMutExt, IoVectoredBuf, IoVectoredBufMut, SetLen};

/// An owned view into a completion buffer.
///
/// Slices are created through [`IoBufExt::slice`]. An open-ended slice follows
/// changes to the underlying buffer's initialized length and capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slice<T> {
    buf: T,
    start: usize,
    end: Option<usize>,
}

impl<T> Slice<T> {
    /// Constructs a slice whose beginning has already been validated.
    ///
    /// # Safety
    ///
    /// `start` must not exceed the underlying buffer's initialized length.
    pub(crate) unsafe fn new_unchecked(buf: T, start: usize, end: Option<usize>) -> Self {
        Self { buf, start, end }
    }

    /// Returns the offset at which this view starts.
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }

    /// Returns the configured exclusive end, or `None` for an open-ended view.
    #[inline]
    pub fn end(&self) -> Option<usize> {
        self.end
    }

    /// Returns a reference to the underlying buffer.
    #[inline]
    pub fn get_ref(&self) -> &T {
        &self.buf
    }

    /// Returns a mutable reference to the underlying buffer.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.buf
    }

    /// Unwraps the view and returns the underlying buffer.
    #[inline]
    pub fn into_inner(self) -> T {
        self.buf
    }
}

impl<T: IoBuf> Slice<T> {
    /// Creates a fixed-end internal view after validating its initialized start.
    pub(crate) fn new(buf: T, start: usize, end: usize) -> Self {
        assert!(start <= buf.buf_len(), "slice starts beyond initialized bytes");
        assert!(start <= end, "slice end precedes its start");
        // Safety: start was checked against the initialized length.
        unsafe { Self::new_unchecked(buf, start, Some(end)) }
    }

    fn initialized_end(&self) -> usize {
        self.end.unwrap_or_else(|| self.buf.buf_len()).min(self.buf.buf_len())
    }

    /// Changes the starting of the view.
    pub fn set_start(&mut self, start: usize) {
        assert!(start <= self.buf.buf_len(), "slice starts beyond initialized bytes");
        self.start = start;
    }

    /// Creates another view relative to this view without nesting adapters.
    ///
    /// An unbounded end retains this view's configured end. A bounded end is
    /// clipped to it, matching the behavior of flattening a nested view.
    pub fn slice(self, range: impl RangeBounds<usize>) -> Self {
        let relative_start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n.checked_add(1).expect("out of range"),
            Bound::Unbounded => 0,
        };
        let relative_end = match range.end_bound() {
            Bound::Included(&n) => Some(n.checked_add(1).expect("out of range")),
            Bound::Excluded(&n) => Some(n),
            Bound::Unbounded => None,
        };

        assert!(
            relative_start <= self.as_init().len(),
            "slice starts beyond initialized bytes"
        );
        if let Some(relative_end) = relative_end {
            assert!(relative_start <= relative_end, "slice end precedes its start");
        }

        let start = self.start.checked_add(relative_start).expect("slice offset overflow");
        let end = match (relative_end, self.end) {
            (Some(relative_end), Some(parent_end)) => Some(
                self.start
                    .checked_add(relative_end)
                    .expect("slice offset overflow")
                    .min(parent_end),
            ),
            (Some(relative_end), None) => Some(self.start.checked_add(relative_end).expect("slice offset overflow")),
            (None, parent_end) => parent_end,
        };

        // Safety: the relative start was checked against this view's
        // initialized bytes, which map directly into the underlying buffer.
        unsafe { Self::new_unchecked(self.buf, start, end) }
    }
}

impl<T: IoBufMut> Slice<T> {
    fn capacity_end(&mut self) -> usize {
        let capacity = self.buf.buf_capacity();
        self.end.unwrap_or(capacity).min(capacity)
    }
}

impl<T: IoBuf> Slice<Slice<T>> {
    /// Flattens nested views without copying the underlying buffer.
    pub fn flatten(self) -> Slice<T> {
        let outer_start = self.buf.start;
        let start = outer_start.checked_add(self.start).expect("slice offset overflow");
        let end = match (self.end, self.buf.end) {
            (Some(inner_end), Some(outer_end)) => Some(
                outer_start
                    .checked_add(inner_end)
                    .expect("slice offset overflow")
                    .min(outer_end),
            ),
            (Some(inner_end), None) => Some(outer_start.checked_add(inner_end).expect("slice offset overflow")),
            (None, outer_end) => outer_end,
        };

        // Safety: construction of both slices established that their relative
        // starts lie within their initialized views.
        unsafe { Slice::new_unchecked(self.buf.buf, start, end) }
    }
}

impl<T> IntoInner for Slice<T> {
    type Inner = T;

    #[inline]
    fn into_inner(self) -> Self::Inner {
        self.buf
    }
}

impl<T: IoBuf> Deref for Slice<T> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.buf.as_init()[self.start..self.initialized_end()]
    }
}

impl<T: IoBufMut> DerefMut for Slice<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let start = self.start;
        let end = self.initialized_end();
        &mut self.buf.as_mut_slice()[start..end]
    }
}

impl<T: IoBuf> IoBuf for Slice<T> {
    #[inline]
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl<T: IoBufMut> IoBufMut for Slice<T> {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        let start = self.start;
        let end = self.capacity_end();
        &mut self.buf.as_uninit()[start..end]
    }

    fn reserve(&mut self, additional: usize) -> std::io::Result<()> {
        if self.end.is_some() {
            let initialized = <Self as IoBuf>::as_init(self).len();
            let capacity = self.as_uninit().len();
            return if additional <= capacity.saturating_sub(initialized) {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "fixed slice cannot reserve additional capacity",
                ))
            };
        }
        self.buf.reserve(additional)
    }
}

impl<T: SetLen> SetLen for Slice<T> {
    unsafe fn set_len(&mut self, len: usize) {
        let absolute = self.start.checked_add(len).expect("buffer length overflow");
        // Safety: the caller initialized the slice prefix, which maps directly
        // to start..start+len in the underlying buffer.
        unsafe { self.buf.set_len(absolute) }
    }
}

/// An owned cursor into a vectored buffer collection.
pub struct VectoredSlice<T> {
    buf: T,
    start: usize,
    index: usize,
    offset: usize,
}

impl<T> VectoredSlice<T> {
    /// Constructs a capacity-based view from already computed offsets.
    pub(crate) fn from_capacity_mut(buf: T, start: usize, index: usize, offset: usize) -> Self {
        Self {
            buf,
            start,
            index,
            offset,
        }
    }

    pub(crate) fn from_initialized(buf: T, start: usize) -> Self
    where
        T: IoVectoredBuf,
    {
        assert!(
            start <= buf.total_len(),
            "vectored slice starts beyond initialized bytes"
        );
        let mut remaining = start;
        let mut index = 0;
        for slice in buf.iter_slice() {
            if remaining < slice.len() {
                break;
            }
            remaining -= slice.len();
            index += 1;
        }
        Self {
            buf,
            start,
            index,
            offset: remaining,
        }
    }

    /// Returns the skipped byte count in the underlying collection.
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }

    /// Returns the underlying buffer collection.
    #[inline]
    pub fn into_inner(self) -> T {
        self.buf
    }
}

impl<T> IntoInner for VectoredSlice<T> {
    type Inner = T;

    #[inline]
    fn into_inner(self) -> Self::Inner {
        self.buf
    }
}

impl<T: IoVectoredBuf> IoVectoredBuf for VectoredSlice<T> {
    fn iter_slice(&self) -> impl Iterator<Item = &[u8]> {
        let index = self.index;
        let offset = self.offset;
        self.buf
            .iter_slice()
            .enumerate()
            .skip(index)
            .map(move |(current, buf)| {
                if current == index {
                    &buf[offset.min(buf.len())..]
                } else {
                    buf
                }
            })
    }
}

impl<T: IoVectoredBufMut> IoVectoredBufMut for VectoredSlice<T> {
    fn iter_uninit_slice(&mut self) -> impl Iterator<Item = &mut [MaybeUninit<u8>]> {
        let index = self.index;
        let offset = self.offset;
        self.buf
            .iter_uninit_slice()
            .enumerate()
            .skip(index)
            .map(move |(current, buf)| {
                if current == index {
                    let offset = offset.min(buf.len());
                    &mut buf[offset..]
                } else {
                    buf
                }
            })
    }
}

impl<T: IoVectoredBufMut> SetLen for VectoredSlice<T> {
    unsafe fn set_len(&mut self, len: usize) {
        let total = self.start.checked_add(len).expect("vectored buffer length overflow");
        // Safety: the caller initialized `len` bytes after the view's starting
        // position, so the underlying initialized prefix ends at start + len.
        unsafe { self.buf.set_len(total) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_ended_slice_follows_vec_capacity() {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(b"abc");
        let mut slice = IoBufExt::slice(buf, 3..);

        assert_eq!(slice.as_init(), b"");
        assert_eq!(slice.buf_capacity(), 5);

        slice.put_slice(b"de");
        assert_eq!(slice.as_init(), b"de");
        assert_eq!(slice.into_inner(), b"abcde");
    }

    #[test]
    fn nested_slices_flatten_offsets() {
        let nested = IoBufExt::slice(IoBufExt::slice(Vec::from(b"abcdef"), 1..5), 2..);
        let flat = nested.flatten();

        assert_eq!(flat.start(), 3);
        assert_eq!(flat.as_init(), b"de");
        assert_eq!(flat.into_inner(), b"abcdef");
    }

    #[test]
    fn chained_slices_stay_flat() {
        fn assert_flat(_: &Slice<Vec<u8>>) {}

        let slice = IoBufExt::slice(Vec::from(b"abcdefgh"), 1..7).slice(2..6).slice(1..);

        assert_flat(&slice);
        assert_eq!(slice.start(), 4);
        assert_eq!(slice.as_init(), b"efg");
        assert_eq!(slice.into_inner(), b"abcdefgh");
    }

    #[test]
    fn vectored_slice_skips_initialized_bytes_without_copying() {
        let bufs = [Vec::from(b"abc"), Vec::from(b"def")];
        let view = IoVectoredBuf::slice(bufs, 4);
        let slices = view.iter_slice().collect::<Vec<_>>();

        assert_eq!(slices, [b"ef".as_slice()]);
        assert_eq!(view.into_inner(), [b"abc".to_vec(), b"def".to_vec()]);
    }
}
