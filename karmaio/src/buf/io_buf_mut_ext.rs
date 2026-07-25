use std::ops::{Bound, RangeBounds};

use crate::buf::IoBufMut;

/// Extension methods for growable / manipulable owned buffers used by framed I/O.
///
/// The core [`IoBufMut`] trait only exposes pointer + init metadata for completion submissions.
/// Framing (length prefixes, delimiters) and codecs need safe ways to clear, grow, append, and shift bytes.
/// Implement this for any buffer type that should work with [`crate::io::framed`].
pub trait IoBufMutExt: IoBufMut {
    /// Clears the initialized region without releasing capacity.
    fn clear(&mut self);

    /// Shrinks the initialized length to `len` without releasing capacity.
    ///
    /// # Panics
    ///
    /// Panics if `len` is greater than the current initialized length.
    fn truncate(&mut self, len: usize);

    /// Ensures at least `additional` bytes of spare capacity after the initialized region.
    ///
    /// # Panics
    ///
    /// May panic if allocation fails (e.g. `Vec::reserve`).
    fn reserve(&mut self, additional: usize);

    /// Appends `src` to the initialized region, growing capacity if needed.
    ///
    /// # Panics
    ///
    /// May panic if allocation fails.
    fn extend_from_slice(&mut self, src: &[u8]);

    /// Copies a range of the initialized buffer to `dest` (like [`slice::copy_within`]).
    ///
    /// Grows the initialized length if the destination end extends past the current length
    /// (used when prepending a length field by shifting the payload right).
    ///
    /// # Panics
    ///
    /// Panics if the source range is out of the current initialized bounds,
    /// or if the destination end exceeds capacity after reservation.
    fn copy_within(&mut self, src: impl RangeBounds<usize>, dest: usize);

    /// Returns the initialized portion of the buffer.
    fn as_init(&self) -> &[u8] {
        // Safety: IoBuf requires stable_read_ptr valid for bytes_init().
        unsafe { std::slice::from_raw_parts(self.stable_read_ptr(), self.bytes_init()) }
    }

    /// Returns a mutable view of the initialized portion of the buffer.
    fn as_mut_init(&mut self) -> &mut [u8] {
        let len = self.bytes_init();
        // Safety: IoBufMut requires stable_write_ptr valid; first `len` bytes are init.
        unsafe { std::slice::from_raw_parts_mut(self.stable_write_ptr(), len) }
    }
}

fn range_bounds(range: impl RangeBounds<usize>, default_end: usize) -> (usize, usize) {
    let start = match range.start_bound() {
        Bound::Included(&n) => n,
        Bound::Excluded(&n) => n.checked_add(1).expect("out of range"),
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&n) => n.checked_add(1).expect("out of range"),
        Bound::Excluded(&n) => n,
        Bound::Unbounded => default_end,
    };
    (start, end)
}

impl IoBufMutExt for Vec<u8> {
    #[inline]
    fn clear(&mut self) {
        Vec::clear(self);
    }

    #[inline]
    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        Vec::reserve(self, additional);
    }

    #[inline]
    fn extend_from_slice(&mut self, src: &[u8]) {
        Vec::extend_from_slice(self, src);
    }

    fn copy_within(&mut self, src: impl RangeBounds<usize>, dest: usize) {
        let init = self.len();
        let (start, end) = range_bounds(src, init);
        assert!(start <= end, "invalid copy_within source range");
        assert!(end <= init, "copy_within source out of initialized bounds");

        let src_len = end - start;
        let dest_end = dest.checked_add(src_len).expect("out of range");

        if dest_end > self.capacity() {
            self.reserve(dest_end - self.capacity());
        }

        // Grow init so both source and destination lie in a valid slice, then shift.
        // Newly exposed bytes between old init and dest are zero-filled by resize.
        if dest_end > self.len() {
            self.resize(dest_end, 0);
        }

        <[u8]>::copy_within(self.as_mut_slice(), start..end, dest);
    }
}
