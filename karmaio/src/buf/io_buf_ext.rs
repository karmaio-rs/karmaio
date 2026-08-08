use std::ops::{Bound, RangeBounds};

use crate::buf::{IoBuf, Slice};

/// Convenience methods for immutable completion buffers.
pub trait IoBufExt: IoBuf {
    /// Returns the number of initialized bytes.
    #[inline]
    fn buf_len(&self) -> usize {
        self.as_init().len()
    }

    /// Returns a readonly pointer to the initialized bytes.
    #[inline]
    fn buf_read_ptr(&self) -> *const u8 {
        self.as_init().as_ptr()
    }

    /// Returns whether the initialized portion is empty.
    #[inline]
    fn is_empty(&self) -> bool {
        self.as_init().is_empty()
    }

    /// Creates an owned view beginning within the initialized bytes.
    ///
    /// An unbounded end continues to follow the underlying buffer's current initialized length or capacity.
    ///
    /// This method is similar to Rust's slicing (`&buf[..]`), but takes ownership of the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use karmaio::buf::{IoBuf, IoBufExt};
    ///
    /// let buf = b"hello world";
    /// assert_eq!(buf.slice(6..).as_init(), b"world");
    /// ```
    ///
    /// # Panics
    /// Panics if:
    /// * begin > buf_len()
    /// * end < begin
    fn slice(self, range: impl RangeBounds<usize>) -> Slice<Self>
    where
        Self: Sized,
    {
        let start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n.checked_add(1).expect("out of range"),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&n) => Some(n.checked_add(1).expect("out of range")),
            Bound::Excluded(&n) => Some(n),
            Bound::Unbounded => None,
        };

        assert!(start <= self.buf_len(), "slice starts beyond initialized bytes");
        if let Some(end) = end {
            assert!(start <= end, "slice end precedes its start");
        }

        // Safety: start was checked against the initialized length.
        unsafe { Slice::new_unchecked(self, start, end) }
    }
}

impl<B: IoBuf + ?Sized> IoBufExt for B {}
