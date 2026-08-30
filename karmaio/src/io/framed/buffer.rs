use std::{io, ops::Range};

use crate::buf::{IoBufMut, IoBufMutExt, Slice};

const DEFAULT_RESERVE: usize = 16;

/// Progress tracker over an owned buffer for framed reads.
///
/// ```text
/// +------------------ capacity ------------------+
/// | consumed |   pending frames   |  uninit     |
/// +----------+--------------------+-------------+
///            ^ Slice start        ^ bytes_init
/// ```
///
/// The stored [`Slice`] view starts at the first unconsumed byte and ends at the
/// buffer capacity so uninit space remains available for the next fill.
pub(super) struct ReadBuffer<B = Vec<u8>>(Option<Slice<B>>);

impl ReadBuffer<Vec<u8>> {
    pub(super) fn new() -> Self {
        Self::new_with(Vec::new())
    }

    pub(super) fn with_capacity(cap: usize) -> Self {
        Self::new_with(Vec::with_capacity(cap))
    }
}

impl<B: IoBufMut + IoBufMutExt> ReadBuffer<B> {
    pub(super) fn new_with(mut buf: B) -> Self {
        let end = buf.as_uninit().len().max(buf.as_init().len());
        Self(Some(Slice::new(buf, 0, end)))
    }

    #[inline]
    pub(super) fn pending(&self) -> &Slice<B> {
        self.0.as_ref().expect("ReadBuffer in inconsistent state")
    }

    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        // Pending initialized bytes: slice start..min(end, bytes_init).
        self.pending().is_empty()
    }

    #[inline]
    pub(super) fn take_inner(&mut self) -> Slice<B> {
        self.0.take().expect("ReadBuffer in inconsistent state")
    }

    #[inline]
    pub(super) fn restore_inner(&mut self, buf: Slice<B>) {
        debug_assert!(self.0.is_none());
        self.0 = Some(buf);
    }

    /// Restores a buffer with an absolute pending cursor `[start, end)`.
    pub(super) fn restore_from_parts(&mut self, mut buf: B, start: usize) {
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        let start = start.min(init);
        self.restore_inner(Slice::new(buf, start, end));
    }

    /// Ensures spare capacity for reading more bytes into the uninitialized tail.
    pub(super) fn reserve(&mut self, additional: usize) -> io::Result<()> {
        let slice = self.take_inner();
        let start = slice.start();
        let mut buf = slice.into_inner();
        let init = buf.as_init().len();
        let spare = buf.as_uninit().len().saturating_sub(init);
        if spare < additional
            && let Err(error) = buf.reserve(additional)
        {
            self.restore_from_parts(buf, start);
            return Err(error);
        }
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        let start = start.min(init);
        self.restore_inner(Slice::new(buf, start, end));
        Ok(())
    }

    /// Compacts a large consumed prefix and returns an owned fill slice.
    ///
    /// Returns `(pending_start, fill_slice)`. After the read completes, call
    /// [`Self::finish_fill`] with the same `pending_start` and the number read.
    pub(super) fn prepare_fill(&mut self) -> io::Result<(usize, Slice<B>)> {
        let slice = self.take_inner();
        let mut pending_start = slice.start();
        let mut buf = slice.into_inner();
        let init = buf.as_init().len();

        // Compact when half the initialized region has been consumed.
        if pending_start > 0 && pending_start >= init / 2 {
            if pending_start < init {
                let pending_len = init - pending_start;
                buf.copy_within(pending_start..init, 0);
                buf.truncate(pending_len);
            } else {
                buf.clear();
            }
            pending_start = 0;
        }

        self.restore_from_parts(buf, pending_start);
        self.reserve(DEFAULT_RESERVE)?;

        let slice = self.take_inner();
        let pending_start = slice.start();
        let mut buf = slice.into_inner();
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        // Fill window is the uninitialized tail [init, capacity).
        let fill = Slice::new(buf, init, end);
        Ok((pending_start, fill))
    }

    /// Restores the read cursor after a fill.
    pub(super) fn finish_fill(&mut self, fill: Slice<B>, pending_start: usize) {
        let mut buf = fill.into_inner();
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        let start = pending_start.min(init);
        self.restore_inner(Slice::new(buf, start, end));
    }

    /// Returns the underlying buffer, escaping the cursor view.
    #[allow(dead_code)]
    pub(super) fn get_ref(&self) -> &B {
        self.pending().get_ref()
    }

    /// Decomposes the buffer into the owned inner buffer and the unread byte range.
    pub(super) fn into_parts(mut self) -> (B, Range<usize>) {
        let slice = self.take_inner();
        let start = slice.start();
        let buf = slice.into_inner();
        let end = buf.as_init().len();
        (buf, start..end)
    }

    /// Reconstructs a `ReadBuffer` from a raw buffer and an unread range.
    ///
    /// The range must satisfy `start <= end` and `end == buf.as_init().len()`.
    pub(super) fn from_parts(mut buf: B, unread: Range<usize>) -> io::Result<Self> {
        let initialized = buf.as_init().len();
        if unread.start > unread.end || unread.end != initialized {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unread range must end at the initialized buffer length",
            ));
        }
        let end = buf.as_uninit().len().max(initialized);
        Ok(Self(Some(Slice::new(buf, unread.start, end))))
    }
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::*;
    use crate::buf::{IoBuf, SetLen};

    struct FixedBuffer {
        bytes: Box<[MaybeUninit<u8>]>,
        initialized: usize,
    }

    impl FixedBuffer {
        fn with_capacity(capacity: usize) -> Self {
            Self {
                bytes: Box::new_uninit_slice(capacity),
                initialized: 0,
            }
        }

        fn initialized(capacity: usize) -> Self {
            let mut buffer = Self::with_capacity(capacity);
            buffer.bytes.fill(MaybeUninit::new(0));
            buffer.initialized = capacity;
            buffer
        }
    }

    impl IoBuf for FixedBuffer {
        fn as_init(&self) -> &[u8] {
            // Safety: the initialized prefix is maintained by SetLen.
            unsafe { std::slice::from_raw_parts(self.bytes.as_ptr().cast(), self.initialized) }
        }
    }

    impl SetLen for FixedBuffer {
        unsafe fn set_len(&mut self, len: usize) {
            assert!(len <= self.bytes.len());
            self.initialized = len;
        }
    }

    impl IoBufMut for FixedBuffer {
        fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
            &mut self.bytes
        }
    }

    #[test]
    fn failed_reserve_preserves_the_owned_buffer() {
        let mut buffer = ReadBuffer::new_with(FixedBuffer::with_capacity(4));

        let error = buffer.reserve(8).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(buffer.get_ref().bytes.len(), 4);
        assert!(buffer.is_empty());
    }

    #[test]
    fn prepare_fill_compacts_before_requesting_growth() {
        let mut buffer = ReadBuffer::new_with(FixedBuffer::initialized(DEFAULT_RESERVE));
        let slice = buffer.take_inner();
        let buf = slice.into_inner();
        buffer.restore_from_parts(buf, DEFAULT_RESERVE);

        let (pending_start, mut fill) = buffer.prepare_fill().unwrap();

        assert_eq!(pending_start, 0);
        assert_eq!(fill.start(), 0);
        assert!(fill.as_init().is_empty());
        assert_eq!(fill.buf_capacity(), DEFAULT_RESERVE);
    }
}
