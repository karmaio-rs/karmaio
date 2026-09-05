use std::{io, mem};

use crate::{
    buf::{IoBuf, IoBufMut, IoBufMutExt},
    io::{
        AsyncWrite, AsyncWriteExt, Sink,
        framed::{PinBoxFuture, codec::Encoder, frame::Framer},
    },
};

pub(super) struct WriteCompletion<W, B> {
    io: W,
    buffer: B,
    result: io::Result<()>,
}

pub(super) enum WriteIoState<W, B> {
    Idle { io: W, buffer: B },
    Writing(PinBoxFuture<WriteCompletion<W, B>>),
    Transitioning,
}

impl<W, B> WriteIoState<W, B> {
    pub(super) fn io(&self) -> Option<&W> {
        match self {
            Self::Idle { io, .. } => Some(io),
            Self::Writing(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn io_mut(&mut self) -> Option<&mut W> {
        match self {
            Self::Idle { io, .. } => Some(io),
            Self::Writing(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn buffer(&self) -> Option<&B> {
        match self {
            Self::Idle { buffer, .. } => Some(buffer),
            Self::Writing(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn buffer_mut(&mut self) -> Option<&mut B> {
        match self {
            Self::Idle { buffer, .. } => Some(buffer),
            Self::Writing(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn is_writing(&self) -> bool {
        matches!(self, Self::Writing(_))
    }
}

/// Lossless components of a settled [`FramedWrite`].
#[derive(Debug)]
pub struct FramedWriteParts<W, C, F, B> {
    /// Underlying transport.
    pub io: W,
    /// Payload encoder.
    pub codec: C,
    /// Byte-level framer.
    pub framer: F,
    /// Reusable write scratch buffer, retaining the encoded frame after a
    /// failed write.
    pub buffer: B,
}

/// Result of settling and decomposing a [`FramedWrite`].
#[derive(Debug)]
pub struct SettledFramedWriteParts<W, C, F, B> {
    /// Recovered writer components.
    pub parts: FramedWriteParts<W, C, F, B>,
    /// Result of the retained write, if one was active.
    pub write_result: io::Result<()>,
}

/// A completion-native framed writer.
///
/// An in-flight owned-buffer write is retained in the adapter.
/// Dropping [`Sink::send`] pauses that operation; the next sink method resumes it before starting more work.
pub struct FramedWrite<W, C, F, B = Vec<u8>> {
    pub(super) codec: C,
    pub(super) framer: F,
    state: WriteIoState<W, B>,
}

impl<W, C, F> FramedWrite<W, C, F, Vec<u8>> {
    /// Creates a new framed writer with an empty scratch buffer.
    pub fn new(io: W, codec: C, framer: F) -> Self {
        Self {
            codec,
            framer,
            state: WriteIoState::Idle { io, buffer: Vec::new() },
        }
    }

    /// Creates a new framed writer with the given scratch capacity.
    pub fn with_capacity(io: W, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            codec,
            framer,
            state: WriteIoState::Idle {
                io,
                buffer: Vec::with_capacity(capacity),
            },
        }
    }
}

impl<W, C, F, B> FramedWrite<W, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates a framed writer using the provided scratch buffer.
    pub fn with_buffer(io: W, codec: C, framer: F, buffer: B) -> Self {
        Self {
            codec,
            framer,
            state: WriteIoState::Idle { io, buffer },
        }
    }

    /// Returns the transport when no write is in progress.
    #[inline]
    pub fn get_ref(&self) -> Option<&W> {
        self.state.io()
    }

    /// Returns the transport mutably when no write is in progress.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut W> {
        self.state.io_mut()
    }

    /// Returns whether an owned-buffer write is retained by the adapter.
    #[inline]
    pub fn is_writing(&self) -> bool {
        self.state.is_writing()
    }

    /// Returns the encoder.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns the encoder mutably.
    #[inline]
    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// Returns the framer.
    #[inline]
    pub fn framer(&self) -> &F {
        &self.framer
    }

    /// Returns the framer mutably.
    #[inline]
    pub fn framer_mut(&mut self) -> &mut F {
        &mut self.framer
    }

    /// Returns the write scratch bytes when no write is in progress.
    ///
    /// The slice is empty after a successful write and retains the complete
    /// encoded frame after a failed write.
    #[inline]
    pub fn write_buffer(&self) -> Option<&[u8]> {
        self.state.buffer().map(IoBuf::as_init)
    }

    /// Returns the write scratch buffer mutably when no write is in progress.
    #[inline]
    pub fn write_buffer_mut(&mut self) -> Option<&mut B> {
        self.state.buffer_mut()
    }

    /// Maps the encoder while preserving retained I/O state.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> FramedWrite<W, C2, F, B> {
        FramedWrite {
            codec: map(self.codec),
            framer: self.framer,
            state: self.state,
        }
    }

    /// Maps the framer while preserving retained I/O state.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> FramedWrite<W, C, F2, B> {
        FramedWrite {
            codec: self.codec,
            framer: map(self.framer),
            state: self.state,
        }
    }

    /// Decomposes the writer immediately if no write is in progress.
    pub fn try_into_parts(self) -> Result<FramedWriteParts<W, C, F, B>, Self> {
        match self.state {
            WriteIoState::Idle { io, buffer } => Ok(FramedWriteParts {
                io,
                codec: self.codec,
                framer: self.framer,
                buffer,
            }),
            WriteIoState::Writing(future) => Err(Self {
                codec: self.codec,
                framer: self.framer,
                state: WriteIoState::Writing(future),
            }),
            WriteIoState::Transitioning => unreachable!("framed writer was left in transition"),
        }
    }

    /// Rebuilds a writer from settled components.
    pub fn from_parts(parts: FramedWriteParts<W, C, F, B>) -> Self {
        Self {
            codec: parts.codec,
            framer: parts.framer,
            state: WriteIoState::Idle {
                io: parts.io,
                buffer: parts.buffer,
            },
        }
    }
}

impl<W, C, F, B> FramedWrite<W, C, F, B> {
    /// Settles any retained write and recovers all components.
    pub async fn into_parts(mut self) -> SettledFramedWriteParts<W, C, F, B> {
        let write_result = settle_write(&mut self.state).await.unwrap_or(Ok(()));
        let WriteIoState::Idle { io, buffer } = self.state else {
            unreachable!("framed writer did not settle")
        };
        SettledFramedWriteParts {
            parts: FramedWriteParts {
                io,
                codec: self.codec,
                framer: self.framer,
                buffer,
            },
            write_result,
        }
    }
}

impl<W, C, F, B, Item> Sink<Item> for FramedWrite<W, C, F, B>
where
    W: AsyncWrite + 'static,
    C: Encoder<Item, B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    type Error = C::Error;

    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        send_item(&mut self.state, &mut self.codec, &mut self.framer, item).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        flush_write(&mut self.state).await.map_err(Into::into)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        close_write(&mut self.state).await.map_err(Into::into)
    }
}

pub(super) async fn send_item<W, C, F, B, Item>(
    state: &mut WriteIoState<W, B>,
    codec: &mut C,
    framer: &mut F,
    item: Item,
) -> Result<(), C::Error>
where
    W: AsyncWrite + 'static,
    C: Encoder<Item, B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    if let Some(result) = settle_write(state).await {
        result.map_err(C::Error::from)?;
    }

    let buffer = state.buffer_mut().expect("framed writer was not idle after settling");
    buffer.clear();
    if !IoBuf::as_init(buffer).is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "framed write buffers must support an empty initialized state",
        )
        .into());
    }
    if let Err(error) = codec.encode(item, buffer) {
        buffer.clear();
        return Err(error);
    }
    if let Err(error) = framer.enclose(buffer) {
        buffer.clear();
        return Err(error.into());
    }

    let previous = mem::replace(state, WriteIoState::Transitioning);
    let WriteIoState::Idle { mut io, buffer } = previous else {
        unreachable!("framed writer was not idle after encoding")
    };
    *state = WriteIoState::Writing(Box::pin(async move {
        let (result, mut buffer) = io.write_all(buffer).await.into_parts();
        if result.is_ok() {
            buffer.clear();
        }
        WriteCompletion {
            io,
            buffer,
            result: result.map(|_| ()),
        }
    }));
    settle_write(state)
        .await
        .expect("write was just started")
        .map_err(Into::into)
}

pub(super) async fn flush_write<W, B>(state: &mut WriteIoState<W, B>) -> io::Result<()>
where
    W: AsyncWrite,
{
    if let Some(result) = settle_write(state).await {
        result?;
    }
    state
        .io_mut()
        .expect("settled writer is missing its transport")
        .flush()
        .await
}

pub(super) async fn close_write<W, B>(state: &mut WriteIoState<W, B>) -> io::Result<()>
where
    W: AsyncWrite,
{
    flush_write(state).await?;
    state
        .io_mut()
        .expect("settled writer is missing its transport")
        .shutdown()
        .await
}

pub(super) async fn settle_write<W, B>(state: &mut WriteIoState<W, B>) -> Option<io::Result<()>> {
    let completion = match state {
        WriteIoState::Writing(future) => future.as_mut().await,
        WriteIoState::Idle { .. } => return None,
        WriteIoState::Transitioning => unreachable!("framed writer was left in transition"),
    };
    let result = completion.result;
    *state = WriteIoState::Idle {
        io: completion.io,
        buffer: completion.buffer,
    };
    Some(result)
}
