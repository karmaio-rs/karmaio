use crate::buf::{IoBufMut, IoBufMutExt, Slice};

const DEFAULT_RESERVE: usize = 8 * 1024;

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
    pub(super) fn reserve(&mut self, additional: usize) {
        let slice = self.take_inner();
        let start = slice.start();
        let mut buf = slice.into_inner();
        let init = buf.as_init().len();
        let spare = buf.as_uninit().len().saturating_sub(init);
        if spare < additional {
            buf.reserve(additional - spare)
                .expect("failed to reserve framed read buffer");
        }
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        let start = start.min(init);
        self.restore_inner(Slice::new(buf, start, end));
    }

    /// Compacts a large consumed prefix and returns an owned fill slice.
    ///
    /// Returns `(pending_start, fill_slice)`. After the read completes, call
    /// [`Self::finish_fill`] with the same `pending_start` and the number read.
    pub(super) fn prepare_fill(&mut self) -> (usize, Slice<B>) {
        self.reserve(DEFAULT_RESERVE);

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

        let init = buf.as_init().len();
        if init >= buf.as_uninit().len() {
            buf.reserve(DEFAULT_RESERVE)
                .expect("failed to reserve framed read buffer");
        }
        let init = buf.as_init().len();
        let end = buf.as_uninit().len().max(init);
        // Fill window is the uninitialized tail [init, capacity).
        let fill = Slice::new(buf, init, end);
        (pending_start, fill)
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
}
