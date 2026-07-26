use crate::{
    buf::{IoBufMut, IoBufMutExt, Slice},
    io::{
        AsyncRead, Stream,
        framed::{buffer::ReadBuffer, codec::Decoder, frame::Framer},
    },
};

/// Read-side state machine for framed decoding
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadState {
    // Actively framing / decoding from the buffer (or need more data).
    Framing,
    // EOF seen once; draining residual frames.
    Pausing,
    // Fully paused after EOF drain; further reads may resume if more data arrives.
    Paused,
    // Last operation errored; next `next` yields `None` and recovers to Paused.
    Errored,
}

/// A framed reader that adapts an [`AsyncRead`] into a [`Stream`] of decoded items.
///
/// Uses a [`Framer`] to locate frames in the byte stream and a [`Decoder`] to turn
/// each payload into a typed value. The owned buffer type defaults to `Vec<u8>`
/// but can be any `B: IoBufMut + IoBufMutExt`.
pub struct FramedRead<R, C, F, B = Vec<u8>> {
    pub(super) io: R,
    pub(super) codec: C,
    pub(super) framer: F,
    pub(super) read: ReadBuffer<B>,
    pub(super) state: ReadState,
}

impl<R, C, F> FramedRead<R, C, F, Vec<u8>> {
    // Creates a new framed reader with a default buffer capacity.
    pub fn new(io: R, codec: C, framer: F) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::new(),
            state: ReadState::Framing,
        }
    }

    /// Creates a new framed reader with the given initial buffer capacity.
    pub fn with_capacity(io: R, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::with_capacity(capacity),
            state: ReadState::Framing,
        }
    }
}

impl<R, C, F, B> FramedRead<R, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates a new framed reader using the provided owned buffer.
    pub fn with_buffer(io: R, codec: C, framer: F, buf: B) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::new_with(buf),
            state: ReadState::Framing,
        }
    }

    /// Returns a reference to the underlying I/O object.
    #[inline]
    pub fn get_ref(&self) -> &R {
        &self.io
    }

    /// Returns a mutable reference to the underlying I/O object.
    ///
    /// Care should be taken not to corrupt the frame stream by reading from it.
    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.io
    }

    /// Consumes the framed reader, returning the underlying I/O object.
    #[inline]
    pub fn into_inner(self) -> R {
        self.io
    }

    /// Returns a reference to the decoder.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns a mutable reference to the decoder.
    #[inline]
    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// Returns a reference to the framer.
    #[inline]
    pub fn framer(&self) -> &F {
        &self.framer
    }

    /// Returns a mutable reference to the framer.
    #[inline]
    pub fn framer_mut(&mut self) -> &mut F {
        &mut self.framer
    }

    /// Returns a view of the pending (unread) buffered data.
    #[inline]
    pub fn read_buffer(&self) -> &[u8] {
        self.read.pending()
    }

    /// Maps the decoder to another type, preserving the buffer and framer.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> FramedRead<R, C2, F, B> {
        FramedRead {
            io: self.io,
            codec: map(self.codec),
            framer: self.framer,
            read: self.read,
            state: self.state,
        }
    }

    /// Maps the framer to another type, preserving the buffer and codec.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> FramedRead<R, C, F2, B> {
        FramedRead {
            io: self.io,
            codec: self.codec,
            framer: map(self.framer),
            read: self.read,
            state: self.state,
        }
    }
}

impl<R, C, F, B> Stream for FramedRead<R, C, F, B>
where
    R: AsyncRead,
    C: Decoder<B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt,
{
    type Item = Result<C::Item, C::Error>;

    async fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.state {
                ReadState::Errored => {
                    self.state = ReadState::Paused;
                    return None;
                }
                ReadState::Paused => match self.fill().await {
                    Ok(0) => return None,
                    Ok(_) => self.state = ReadState::Framing,
                    Err(e) => {
                        self.state = ReadState::Errored;
                        return Some(Err(e.into()));
                    }
                },
                ReadState::Pausing => match self.try_decode_eof() {
                    Ok(Some(item)) => return Some(Ok(item)),
                    Ok(None) => {
                        self.state = ReadState::Paused;
                        return None;
                    }
                    Err(e) => {
                        self.state = ReadState::Errored;
                        return Some(Err(e));
                    }
                },
                ReadState::Framing => {
                    if !self.read.is_empty() {
                        match self.try_decode() {
                            Ok(Some(item)) => return Some(Ok(item)),
                            Ok(None) => {}
                            Err(e) => {
                                self.state = ReadState::Errored;
                                return Some(Err(e));
                            }
                        }
                    }

                    match self.fill().await {
                        Ok(0) => {
                            self.state = ReadState::Pausing;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            self.state = ReadState::Errored;
                            return Some(Err(e.into()));
                        }
                    }
                }
            }
        }
    }
}

impl<R, C, F, B> FramedRead<R, C, F, B>
where
    R: AsyncRead,
    C: Decoder<B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt,
{
    async fn fill(&mut self) -> std::io::Result<usize> {
        let (pending_start, fill) = self.read.prepare_fill();
        let (res, fill) = self.io.read(fill).await;
        self.read.finish_fill(fill, pending_start);
        res
    }

    fn try_decode(&mut self) -> Result<Option<C::Item>, C::Error> {
        let frame = {
            let pending = self.read.pending();
            self.framer.extract(pending)?
        };
        self.decode_frame(frame, false)
    }

    fn try_decode_eof(&mut self) -> Result<Option<C::Item>, C::Error> {
        // Drain complete frames first.
        if !self.read.is_empty() {
            if let Some(item) = self.try_decode()? {
                return Ok(Some(item));
            }
        }

        let frame = {
            let pending = self.read.pending();
            self.framer.extract_eof(pending)?
        };
        self.decode_frame(frame, true)
    }

    fn decode_frame(
        &mut self,
        frame: Option<crate::io::framed::Frame>,
        at_eof: bool,
    ) -> Result<Option<C::Item>, C::Error> {
        let Some(frame) = frame else {
            return Ok(None);
        };

        let frame_len = frame.len();
        let slice = self.read.take_inner();
        let begin = slice.start();
        let buf = slice.into_inner();

        // Frame offsets are relative to the pending view (absolute = begin + offset).
        let abs_prefix = begin + frame.prefix();
        let abs_payload_end = abs_prefix + frame.payload();
        let payload = Slice::new(buf, abs_prefix, abs_payload_end);
        let decoded = if at_eof {
            self.codec.decode_eof(&payload)
        } else {
            self.codec.decode(&payload).map(Some)
        };
        let buf = payload.into_inner();

        let frame_end = begin + frame_len;
        self.read.restore_from_parts(buf, frame_end);

        decoded
    }
}
