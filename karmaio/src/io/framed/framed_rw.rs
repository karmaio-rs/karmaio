use crate::{
    buf::{IoBufMut, IoBufMutExt, Slice},
    io::{
        AsyncRead, AsyncWrite, AsyncWriteExt, Sink, Stream,
        framed::{
            buffer::ReadBuffer,
            codec::{Decoder, Encoder},
            frame::{Frame, Framer},
            framed_read::ReadState,
        },
    },
};

/// A duplex framed I/O adapter providing both [`Stream`] and [`Sink`] over one I/O object.
///
/// Uses a [`Framer`] for byte layout and a codec for payload encode/decode. The owned
/// buffer type defaults to `Vec<u8>` but can be any `B: IoBufMut + IoBufMutExt`.
pub struct Framed<IO, C, F, B = Vec<u8>> {
    io: IO,
    codec: C,
    framer: F,
    read: ReadBuffer<B>,
    write: Option<B>,
    state: ReadState,
}

impl<IO, C, F> Framed<IO, C, F, Vec<u8>> {
    /// Creates a new framed duplex with default buffer capacities.
    pub fn new(io: IO, codec: C, framer: F) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::new(),
            write: Some(Vec::new()),
            state: ReadState::Framing,
        }
    }

    /// Creates a new framed duplex with the given initial buffer capacity (both sides).
    pub fn with_capacity(io: IO, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::with_capacity(capacity),
            write: Some(Vec::with_capacity(capacity)),
            state: ReadState::Framing,
        }
    }
}

impl<IO, C, F, B> Framed<IO, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates a new framed duplex using the provided read and write buffers.
    pub fn with_buffer(io: IO, codec: C, framer: F, read_buf: B, write_buf: B) -> Self {
        Self {
            io,
            codec,
            framer,
            read: ReadBuffer::new_with(read_buf),
            write: Some(write_buf),
            state: ReadState::Framing,
        }
    }

    /// Returns a reference to the underlying I/O object.
    #[inline]
    pub fn get_ref(&self) -> &IO {
        &self.io
    }

    /// Returns a mutable reference to the underlying I/O object.
    #[inline]
    pub fn get_mut(&mut self) -> &mut IO {
        &mut self.io
    }

    /// Consumes the framed adapter, returning the underlying I/O object.
    #[inline]
    pub fn into_inner(self) -> IO {
        self.io
    }

    /// Returns a reference to the codec.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns a mutable reference to the codec.
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

    /// Returns a view of the pending read buffer.
    #[inline]
    pub fn read_buffer(&self) -> &[u8] {
        self.read.pending()
    }

    /// Returns a view of the write scratch buffer.
    #[inline]
    pub fn write_buffer(&self) -> &[u8] {
        self.write.as_ref().map(|b| b.as_init()).unwrap_or(&[])
    }

    /// Maps the codec to another type, preserving buffers and framer.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> Framed<IO, C2, F, B> {
        Framed {
            io: self.io,
            codec: map(self.codec),
            framer: self.framer,
            read: self.read,
            write: self.write,
            state: self.state,
        }
    }

    /// Maps the framer to another type, preserving buffers and codec.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> Framed<IO, C, F2, B> {
        Framed {
            io: self.io,
            codec: self.codec,
            framer: map(self.framer),
            read: self.read,
            write: self.write,
            state: self.state,
        }
    }
}

impl<IO, C, F, B> Stream for Framed<IO, C, F, B>
where
    IO: AsyncRead,
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

impl<IO, C, F, B, Item> Sink<Item> for Framed<IO, C, F, B>
where
    IO: AsyncWrite,
    C: Encoder<Item, B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    type Error = C::Error;

    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        let mut buf = self.write.take().expect("Framed write buffer missing during encode");
        buf.clear();
        if let Err(e) = self.codec.encode(item, &mut buf) {
            buf.clear();
            self.write = Some(buf);
            return Err(e);
        }
        self.framer.enclose(&mut buf);

        let (res, mut buf) = self.io.write_all(buf).await.into_parts();
        buf.clear();
        self.write = Some(buf);
        res.map(|_| ()).map_err(Into::into)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.io.flush().await.map_err(Into::into)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.flush().await?;
        self.io.shutdown().await.map_err(Into::into)
    }
}

impl<IO, C, F, B> Framed<IO, C, F, B>
where
    IO: AsyncRead,
    C: Decoder<B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt,
{
    async fn fill(&mut self) -> std::io::Result<usize> {
        let (pending_start, fill) = self.read.prepare_fill();
        let (res, fill) = self.io.read(fill).await.into_parts();
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
        if !self.read.is_empty()
            && let Some(item) = self.try_decode()?
        {
            return Ok(Some(item));
        }

        let frame = {
            let pending = self.read.pending();
            self.framer.extract_eof(pending)?
        };
        self.decode_frame(frame, true)
    }

    fn decode_frame(&mut self, frame: Option<Frame>, at_eof: bool) -> Result<Option<C::Item>, C::Error> {
        let Some(frame) = frame else {
            return Ok(None);
        };

        let frame_len = frame.len();
        let slice = self.read.take_inner();
        let start = slice.start();
        let buf = slice.into_inner();

        let abs_prefix = start + frame.prefix();
        let abs_payload_end = abs_prefix + frame.payload();
        let payload = Slice::new(buf, abs_prefix, abs_payload_end);
        let decoded = if at_eof {
            self.codec.decode_eof(&payload)
        } else {
            self.codec.decode(&payload).map(Some)
        };
        let buf = payload.into_inner();

        let frame_end = start + frame_len;
        self.read.restore_from_parts(buf, frame_end);

        decoded
    }
}
