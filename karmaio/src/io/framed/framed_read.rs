use std::{io, mem, ops::Range};

use crate::{
    buf::{IoBufMut, IoBufMutExt, Slice},
    io::{
        AsyncRead, Stream,
        framed::{PinBoxFuture, buffer::ReadBuffer, codec::Decoder, frame::Framer},
    },
};

pub(super) struct ReadCompletion<R, B> {
    pub(super) io: R,
    pub(super) buffer: ReadBuffer<B>,
    pub(super) result: io::Result<usize>,
}

pub(super) enum ReadIoState<R, B> {
    Idle { io: R, buffer: ReadBuffer<B> },
    Reading(PinBoxFuture<ReadCompletion<R, B>>),
    Transitioning,
}

impl<R, B> ReadIoState<R, B> {
    pub(super) fn is_reading(&self) -> bool {
        matches!(self, Self::Reading(_))
    }

    pub(super) fn io(&self) -> Option<&R> {
        match self {
            Self::Idle { io, .. } => Some(io),
            Self::Reading(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn io_mut(&mut self) -> Option<&mut R> {
        match self {
            Self::Idle { io, .. } => Some(io),
            Self::Reading(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn buffer(&self) -> Option<&ReadBuffer<B>> {
        match self {
            Self::Idle { buffer, .. } => Some(buffer),
            Self::Reading(_) | Self::Transitioning => None,
        }
    }

    pub(super) fn buffer_mut(&mut self) -> Option<&mut ReadBuffer<B>> {
        match self {
            Self::Idle { buffer, .. } => Some(buffer),
            Self::Reading(_) | Self::Transitioning => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodeState {
    Framing,
    Pausing,
    Paused,
    Errored,
}

/// Buffered components of a settled [`FramedRead`].
#[derive(Debug)]
pub struct FramedReadParts<R, C, F, B> {
    /// Underlying transport.
    pub io: R,
    /// Payload decoder.
    pub codec: C,
    /// Byte-level framer.
    pub framer: F,
    /// Owned read buffer, including any consumed prefix.
    pub read_buf: B,
    /// Initialized unread range within `read_buf`.
    pub unread: Range<usize>,
}

/// Result of settling and decomposing a [`FramedRead`].
#[derive(Debug)]
pub struct SettledFramedReadParts<R, C, F, B> {
    /// Recovered reader components.
    pub parts: FramedReadParts<R, C, F, B>,
    /// Result of the retained read, if one was active.
    pub read_result: io::Result<()>,
}

/// A completion-native framed reader.
///
/// The in-flight read future is retained in the adapter.
/// Dropping [`Stream::next`] pauses rather than destroys the operation, its transport, or its completion buffer.
pub struct FramedRead<R, C, F, B = Vec<u8>> {
    pub(super) codec: C,
    pub(super) framer: F,
    pub(super) read: ReadIoState<R, B>,
    state: DecodeState,
}

impl<R, C, F> FramedRead<R, C, F, Vec<u8>> {
    /// Creates a new framed reader.
    pub fn new(io: R, codec: C, framer: F) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io,
                buffer: ReadBuffer::new(),
            },
            state: DecodeState::Framing,
        }
    }

    /// Creates a framed reader with the given initial buffer capacity.
    pub fn with_capacity(io: R, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io,
                buffer: ReadBuffer::with_capacity(capacity),
            },
            state: DecodeState::Framing,
        }
    }
}

impl<R, C, F, B> FramedRead<R, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates a framed reader using the provided owned buffer.
    pub fn with_buffer(io: R, codec: C, framer: F, buffer: B) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io,
                buffer: ReadBuffer::new_with(buffer),
            },
            state: DecodeState::Framing,
        }
    }

    /// Returns the transport while no read is in progress.
    #[inline]
    pub fn get_ref(&self) -> Option<&R> {
        self.read.io()
    }

    /// Returns the transport mutably while no read is in progress.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut R> {
        self.read.io_mut()
    }

    /// Returns whether an owned-buffer read is retained by the adapter.
    #[inline]
    pub fn is_reading(&self) -> bool {
        self.read.is_reading()
    }

    /// Returns the decoder.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns the decoder mutably.
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

    /// Returns unread bytes while no read is in progress.
    #[inline]
    pub fn read_buffer(&self) -> Option<&[u8]> {
        self.read.buffer().map(|buffer| &buffer.pending()[..])
    }

    /// Returns unread bytes mutably while no read is in progress.
    #[inline]
    pub fn read_buffer_mut(&mut self) -> Option<&mut [u8]> {
        self.read.buffer_mut().map(ReadBuffer::pending_mut)
    }

    /// Maps the decoder while preserving retained I/O state.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> FramedRead<R, C2, F, B> {
        FramedRead {
            codec: map(self.codec),
            framer: self.framer,
            read: self.read,
            state: self.state,
        }
    }

    /// Maps the framer while preserving retained I/O state.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> FramedRead<R, C, F2, B> {
        FramedRead {
            codec: self.codec,
            framer: map(self.framer),
            read: self.read,
            state: self.state,
        }
    }

    /// Decomposes the reader immediately if no read is in progress.
    pub fn try_into_parts(self) -> Result<FramedReadParts<R, C, F, B>, Self> {
        if self.read.is_reading() {
            return Err(self);
        }
        Ok(self.into_parts_unchecked())
    }

    /// Rebuilds a settled reader from buffered components.
    ///
    /// Transient EOF and error state is reset
    pub fn from_parts(parts: FramedReadParts<R, C, F, B>) -> Result<Self, FramedReadParts<R, C, F, B>> {
        let valid = parts.unread.start <= parts.unread.end && parts.unread.end == parts.read_buf.as_init().len();
        if !valid {
            return Err(parts);
        }
        let FramedReadParts {
            io,
            codec,
            framer,
            read_buf,
            unread,
        } = parts;
        let buffer = ReadBuffer::from_parts(read_buf, unread).expect("framed-read parts were validated");
        Ok(Self {
            codec,
            framer,
            read: ReadIoState::Idle { io, buffer },
            state: DecodeState::Framing,
        })
    }

    fn into_parts_unchecked(self) -> FramedReadParts<R, C, F, B> {
        let ReadIoState::Idle { io, buffer } = self.read else {
            unreachable!("in-flight read was not settled")
        };
        let (read_buf, unread) = buffer.into_parts();
        FramedReadParts {
            io,
            codec: self.codec,
            framer: self.framer,
            read_buf,
            unread,
        }
    }
}

impl<R, C, F, B> FramedRead<R, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Settles any retained read and recovers all components.
    pub async fn into_parts(mut self) -> SettledFramedReadParts<R, C, F, B> {
        let read_result = settle_read(&mut self.read).await.transpose().map(|_| ());
        SettledFramedReadParts {
            parts: self.into_parts_unchecked(),
            read_result,
        }
    }
}

impl<R, C, F, B> Stream for FramedRead<R, C, F, B>
where
    R: AsyncRead + 'static,
    C: Decoder<B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    type Item = Result<C::Item, C::Error>;

    async fn next(&mut self) -> Option<Self::Item> {
        next_item(&mut self.read, &mut self.state, &mut self.codec, &mut self.framer).await
    }
}

/// Drives the read state shared by read-only and duplex framed adapters.
pub(super) async fn next_item<R, C, F, B>(
    read: &mut ReadIoState<R, B>,
    state: &mut DecodeState,
    codec: &mut C,
    framer: &mut F,
) -> Option<Result<C::Item, C::Error>>
where
    R: AsyncRead + 'static,
    C: Decoder<B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    loop {
        match state {
            DecodeState::Errored => return None,
            DecodeState::Paused => match fill_read(read).await {
                Ok(0) => return None,
                Ok(_) => *state = DecodeState::Framing,
                Err(error) => {
                    *state = DecodeState::Errored;
                    return Some(Err(error.into()));
                }
            },
            DecodeState::Pausing => match try_decode_eof(read, codec, framer) {
                Ok(Some(item)) => return Some(Ok(item)),
                Ok(None) => {
                    *state = DecodeState::Paused;
                    return None;
                }
                Err(error) => {
                    *state = DecodeState::Errored;
                    return Some(Err(error));
                }
            },
            DecodeState::Framing => {
                if read.buffer().is_some_and(|buffer| !buffer.is_empty()) {
                    match try_decode(read, codec, framer) {
                        Ok(Some(item)) => return Some(Ok(item)),
                        Ok(None) => {}
                        Err(error) => {
                            *state = DecodeState::Errored;
                            return Some(Err(error));
                        }
                    }
                }
                match fill_read(read).await {
                    Ok(0) => *state = DecodeState::Pausing,
                    Ok(_) => {}
                    Err(error) => {
                        *state = DecodeState::Errored;
                        return Some(Err(error.into()));
                    }
                }
            }
        }
    }
}

async fn fill_read<R, B>(read: &mut ReadIoState<R, B>) -> io::Result<usize>
where
    R: AsyncRead + 'static,
    B: IoBufMut + IoBufMutExt + 'static,
{
    if let Some(result) = settle_read(read).await {
        return result;
    }

    let previous = mem::replace(read, ReadIoState::Transitioning);
    let ReadIoState::Idle { mut io, mut buffer } = previous else {
        unreachable!("framed reader was not idle after settling")
    };
    let (pending_start, fill) = match buffer.prepare_fill() {
        Ok(fill) => fill,
        Err(error) => {
            *read = ReadIoState::Idle { io, buffer };
            return Err(error);
        }
    };
    *read = ReadIoState::Reading(Box::pin(async move {
        let (result, fill) = io.read(fill).await.into_parts();
        buffer.finish_fill(fill, pending_start);
        ReadCompletion { io, buffer, result }
    }));
    settle_read(read).await.expect("read was just started")
}

fn try_decode<R, C, F, B>(
    read: &mut ReadIoState<R, B>,
    codec: &mut C,
    framer: &mut F,
) -> Result<Option<C::Item>, C::Error>
where
    B: IoBufMut + IoBufMutExt,
    C: Decoder<B>,
    F: Framer<B>,
{
    let frame = {
        let pending = read.buffer().expect("decoder ran during a read").pending();
        framer.extract(pending)?
    };
    decode_frame(read, codec, frame, false)
}

fn try_decode_eof<R, C, F, B>(
    read: &mut ReadIoState<R, B>,
    codec: &mut C,
    framer: &mut F,
) -> Result<Option<C::Item>, C::Error>
where
    B: IoBufMut + IoBufMutExt,
    C: Decoder<B>,
    F: Framer<B>,
{
    if read.buffer().is_some_and(|buffer| !buffer.is_empty())
        && let Some(item) = try_decode(read, codec, framer)?
    {
        return Ok(Some(item));
    }
    let frame = {
        let pending = read.buffer().expect("EOF decoder ran during a read").pending();
        framer.extract_eof(pending)?
    };
    decode_frame(read, codec, frame, true)
}

fn decode_frame<R, C, B>(
    read: &mut ReadIoState<R, B>,
    codec: &mut C,
    frame: Option<crate::io::framed::Frame>,
    at_eof: bool,
) -> Result<Option<C::Item>, C::Error>
where
    B: IoBufMut + IoBufMutExt,
    C: Decoder<B>,
{
    let Some(frame) = frame else {
        return Ok(None);
    };
    let buffer = read.buffer_mut().expect("frame decoder ran during a read");
    let start = buffer.pending().start();
    let (payload_range, frame_len) = frame.checked_bounds(buffer.pending().len())?;
    let abs_prefix = start
        .checked_add(payload_range.start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame offset overflow"))?;
    let abs_payload_end = start
        .checked_add(payload_range.end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame offset overflow"))?;
    let frame_end = start
        .checked_add(frame_len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame offset overflow"))?;
    let slice = buffer.take_inner();
    let payload = Slice::new(slice.into_inner(), abs_prefix, abs_payload_end);
    let decoded = if at_eof {
        codec.decode_eof(&payload)
    } else {
        codec.decode(&payload).map(Some)
    };
    buffer.restore_from_parts(payload.into_inner(), frame_end);
    decoded
}

pub(super) async fn settle_read<R, B>(state: &mut ReadIoState<R, B>) -> Option<io::Result<usize>> {
    let completion = match state {
        ReadIoState::Reading(future) => future.as_mut().await,
        ReadIoState::Idle { .. } => return None,
        ReadIoState::Transitioning => unreachable!("framed reader was left in transition"),
    };
    let result = completion.result;
    *state = ReadIoState::Idle {
        io: completion.io,
        buffer: completion.buffer,
    };
    Some(result)
}
