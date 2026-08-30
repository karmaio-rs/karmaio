use crate::{
    buf::{IoBufMut, IoBufMutExt},
    io::{
        AsyncWrite, AsyncWriteExt, Sink,
        framed::{codec::Encoder, frame::Framer},
    },
};

/// Lossless components of a [`FramedWrite`], obtained via [`FramedWrite::into_parts`].
#[derive(Debug)]
pub struct FramedWriteParts<W, C, F, B> {
    /// Underlying transport.
    pub io: W,
    /// Payload encoder.
    pub codec: C,
    /// Byte-level framer.
    pub framer: F,
    /// Reusable write scratch buffer.
    pub buffer: B,
}

/// A framed writer that adapts an [`AsyncWrite`] into a [`Sink`] of encoded items.
///
/// Each `send` encodes the item, encloses framing, and immediately `write_all`s
/// the frame. The owned buffer type defaults to `Vec<u8>`.
pub struct FramedWrite<W, C, F, B = Vec<u8>> {
    pub(super) io: W,
    pub(super) codec: C,
    pub(super) framer: F,
    /// `None` only while an in-flight `write_all` owns the buffer.
    pub(super) write: Option<B>,
}

impl<W, C, F> FramedWrite<W, C, F, Vec<u8>> {
    /// Creates a new framed writer with an empty scratch buffer.
    pub fn new(io: W, codec: C, framer: F) -> Self {
        Self {
            io,
            codec,
            framer,
            write: Some(Vec::new()),
        }
    }

    /// Creates a new framed writer with the given scratch buffer capacity.
    pub fn with_capacity(io: W, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            io,
            codec,
            framer,
            write: Some(Vec::with_capacity(capacity)),
        }
    }
}

impl<W, C, F, B> FramedWrite<W, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates a new framed writer using the provided scratch buffer.
    pub fn with_buffer(io: W, codec: C, framer: F, buf: B) -> Self {
        Self {
            io,
            codec,
            framer,
            write: Some(buf),
        }
    }

    /// Returns a reference to the underlying I/O object.
    #[inline]
    pub fn get_ref(&self) -> &W {
        &self.io
    }

    /// Returns a mutable reference to the underlying I/O object.
    #[inline]
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.io
    }

    /// Consumes the framed writer, returning the underlying I/O object.
    #[inline]
    pub fn into_inner(self) -> W {
        self.io
    }

    /// Returns a reference to the encoder.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns a mutable reference to the encoder.
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

    /// Returns a view of the write scratch buffer.
    #[inline]
    pub fn write_buffer(&self) -> &[u8] {
        self.write.as_ref().map(|b| b.as_init()).unwrap_or(&[])
    }

    /// Maps the encoder to another type, preserving the buffer and framer.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> FramedWrite<W, C2, F, B> {
        FramedWrite {
            io: self.io,
            codec: map(self.codec),
            framer: self.framer,
            write: self.write,
        }
    }

    /// Maps the framer to another type, preserving the buffer and codec.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> FramedWrite<W, C, F2, B> {
        FramedWrite {
            io: self.io,
            codec: self.codec,
            framer: map(self.framer),
            write: self.write,
        }
    }

    /// Decomposes the writer into its constituent parts.
    ///
    /// Returns `Err(self)` if a write is in flight (the buffer has been moved
    /// into the underlying I/O).
    pub fn try_into_parts(self) -> Result<FramedWriteParts<W, C, F, B>, Self> {
        match self.write {
            Some(buffer) => Ok(FramedWriteParts {
                io: self.io,
                codec: self.codec,
                framer: self.framer,
                buffer,
            }),
            None => Err(self),
        }
    }

    /// Rebuilds a writer from previously obtained parts.
    pub fn from_parts(parts: FramedWriteParts<W, C, F, B>) -> Self {
        FramedWrite {
            io: parts.io,
            codec: parts.codec,
            framer: parts.framer,
            write: Some(parts.buffer),
        }
    }
}

impl<W, C, F, B, Item> Sink<Item> for FramedWrite<W, C, F, B>
where
    W: AsyncWrite,
    C: Encoder<Item, B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    type Error = C::Error;

    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        let mut buf = self.write.take().expect("FramedWrite buffer missing during encode");
        buf.clear();
        if let Err(e) = self.codec.encode(item, &mut buf) {
            buf.clear();
            self.write = Some(buf);
            return Err(e);
        }
        if let Err(error) = self.framer.enclose(&mut buf) {
            buf.clear();
            self.write = Some(buf);
            return Err(error.into());
        }

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
