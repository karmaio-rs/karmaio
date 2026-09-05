use std::{future::Future, io, ops::Range, task::Poll};

use crate::{
    buf::{IoBuf, IoBufMut, IoBufMutExt},
    io::{
        AsyncRead, AsyncWrite, IntoOwnedSplit, Sink, Stream,
        framed::{
            buffer::ReadBuffer,
            codec::{Decoder, Encoder},
            frame::Framer,
            framed_read::{DecodeState, ReadIoState, next_item, settle_read},
            framed_write::{WriteIoState, close_write, flush_write, send_item, settle_write},
        },
    },
};

/// Buffered components of a settled [`Framed`].
#[derive(Debug)]
pub struct FramedParts<R, W, C, F, B> {
    /// Underlying reader.
    pub reader: R,
    /// Underlying writer.
    pub writer: W,
    /// Payload codec.
    pub codec: C,
    /// Byte-level framer.
    pub framer: F,
    /// Owned read buffer, including any consumed prefix.
    pub read_buf: B,
    /// Initialized unread range within `read_buf`.
    pub unread: Range<usize>,
    /// Reusable write scratch buffer, retaining the encoded frame after a
    /// failed write.
    pub write_buf: B,
}

/// Result of settling and decomposing a [`Framed`].
#[derive(Debug)]
pub struct SettledFramedParts<R, W, C, F, B> {
    /// Recovered framed components.
    pub parts: FramedParts<R, W, C, F, B>,
    /// Result of a retained read, if one was active.
    pub read_result: io::Result<()>,
    /// Result of a retained write, if one was active.
    pub write_result: io::Result<()>,
}

/// A completion-native framed adapter with independent reader and writer halves.
///
/// Each direction retains its own submitted operation, A cancelled read therefore does not prevent a write from making progress, and vice versa.
///
/// The reader and writer must provide independent progress. In particular,
/// they should not be two handles that lock one duplex value for the lifetime
/// of an asynchronous operation. Karmaio's TLS streams do not currently meet
/// this requirement and are therefore not supported by this combined adapter.
pub struct Framed<R, W, C, F, B = Vec<u8>> {
    codec: C,
    framer: F,
    read: ReadIoState<R, B>,
    write: WriteIoState<W, B>,
    state: DecodeState,
}

impl<R, W, C, F> Framed<R, W, C, F, Vec<u8>> {
    /// Creates an adapter over independently owned reader and writer halves.
    pub fn new(reader: R, writer: W, codec: C, framer: F) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io: reader,
                buffer: ReadBuffer::new(),
            },
            write: WriteIoState::Idle {
                io: writer,
                buffer: Vec::new(),
            },
            state: DecodeState::Framing,
        }
    }

    /// Creates an adapter with the given initial read and write capacities.
    pub fn with_capacity(reader: R, writer: W, codec: C, framer: F, capacity: usize) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io: reader,
                buffer: ReadBuffer::with_capacity(capacity),
            },
            write: WriteIoState::Idle {
                io: writer,
                buffer: Vec::with_capacity(capacity),
            },
            state: DecodeState::Framing,
        }
    }
}

impl<C, F> Framed<(), (), C, F, Vec<u8>> {
    /// Splits a duplex transport and creates an adapter over its owned halves.
    ///
    /// [`IntoOwnedSplit`] is reserved for transports whose halves can make
    /// independent progress. Karmaio TLS streams intentionally do not
    /// implement it. Wrapping an unsplittable stream in a lock is not suitable
    /// here because a retained read could prevent the write side from ever
    /// acquiring that lock.
    pub fn with_duplex<IO>(io: IO, codec: C, framer: F) -> Framed<IO::ReadHalf, IO::WriteHalf, C, F>
    where
        IO: IntoOwnedSplit,
    {
        let (reader, writer) = io.into_split();
        Framed::new(reader, writer, codec, framer)
    }

    /// Splits a duplex transport and creates an adapter with initial buffer capacities.
    ///
    /// This has the same independent-progress requirement as
    /// [`Self::with_duplex`].
    pub fn with_duplex_capacity<IO>(
        io: IO,
        codec: C,
        framer: F,
        capacity: usize,
    ) -> Framed<IO::ReadHalf, IO::WriteHalf, C, F>
    where
        IO: IntoOwnedSplit,
    {
        let (reader, writer) = io.into_split();
        Framed::with_capacity(reader, writer, codec, framer, capacity)
    }
}

impl<R, W, C, F, B> Framed<R, W, C, F, B>
where
    B: IoBufMut + IoBufMutExt,
{
    /// Creates an adapter using caller-provided read and write buffers.
    pub fn with_buffer(reader: R, writer: W, codec: C, framer: F, read_buf: B, write_buf: B) -> Self {
        Self {
            codec,
            framer,
            read: ReadIoState::Idle {
                io: reader,
                buffer: ReadBuffer::new_with(read_buf),
            },
            write: WriteIoState::Idle {
                io: writer,
                buffer: write_buf,
            },
            state: DecodeState::Framing,
        }
    }

    /// Returns the reader while no read is in progress.
    #[inline]
    pub fn reader_ref(&self) -> Option<&R> {
        self.read.io()
    }

    /// Returns the reader mutably while no read is in progress.
    #[inline]
    pub fn reader_mut(&mut self) -> Option<&mut R> {
        self.read.io_mut()
    }

    /// Returns the writer while no write is in progress.
    #[inline]
    pub fn writer_ref(&self) -> Option<&W> {
        self.write.io()
    }

    /// Returns the writer mutably while no write is in progress.
    #[inline]
    pub fn writer_mut(&mut self) -> Option<&mut W> {
        self.write.io_mut()
    }

    /// Returns whether an owned-buffer read is retained by the adapter.
    #[inline]
    pub fn is_reading(&self) -> bool {
        self.read.is_reading()
    }

    /// Returns whether an owned-buffer write is retained by the adapter.
    #[inline]
    pub fn is_writing(&self) -> bool {
        self.write.is_writing()
    }

    /// Returns the codec.
    #[inline]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns the codec mutably.
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

    /// Returns the write scratch bytes while no write is in progress.
    #[inline]
    pub fn write_buffer(&self) -> Option<&[u8]> {
        self.write.buffer().map(IoBuf::as_init)
    }

    /// Returns the write scratch buffer mutably while no write is in progress.
    #[inline]
    pub fn write_buffer_mut(&mut self) -> Option<&mut B> {
        self.write.buffer_mut()
    }

    /// Maps the codec while preserving retained operations.
    pub fn map_codec<C2>(self, map: impl FnOnce(C) -> C2) -> Framed<R, W, C2, F, B> {
        Framed {
            codec: map(self.codec),
            framer: self.framer,
            read: self.read,
            write: self.write,
            state: self.state,
        }
    }

    /// Maps the framer while preserving retained operations.
    pub fn map_framer<F2>(self, map: impl FnOnce(F) -> F2) -> Framed<R, W, C, F2, B> {
        Framed {
            codec: self.codec,
            framer: map(self.framer),
            read: self.read,
            write: self.write,
            state: self.state,
        }
    }

    /// Decomposes the adapter immediately if neither direction is active.
    pub fn try_into_parts(self) -> Result<FramedParts<R, W, C, F, B>, Self> {
        if self.read.is_reading() || self.write.is_writing() {
            return Err(self);
        }
        Ok(self.into_parts_unchecked())
    }

    /// Rebuilds a settled adapter from buffered components.
    ///
    /// Transient EOF and read-error state is reset
    pub fn from_parts(parts: FramedParts<R, W, C, F, B>) -> Result<Self, FramedParts<R, W, C, F, B>> {
        let valid = parts.unread.start <= parts.unread.end && parts.unread.end == parts.read_buf.as_init().len();
        if !valid {
            return Err(parts);
        }
        let FramedParts {
            reader,
            writer,
            codec,
            framer,
            read_buf,
            unread,
            write_buf,
        } = parts;
        let buffer = ReadBuffer::from_parts(read_buf, unread).expect("framed parts were validated");
        Ok(Self {
            codec,
            framer,
            read: ReadIoState::Idle { io: reader, buffer },
            write: WriteIoState::Idle {
                io: writer,
                buffer: write_buf,
            },
            state: DecodeState::Framing,
        })
    }

    fn into_parts_unchecked(self) -> FramedParts<R, W, C, F, B> {
        let ReadIoState::Idle { io: reader, buffer } = self.read else {
            unreachable!("framed reader was not settled")
        };
        let WriteIoState::Idle {
            io: writer,
            buffer: write_buf,
        } = self.write
        else {
            unreachable!("framed writer was not settled")
        };
        let (read_buf, unread) = buffer.into_parts();
        FramedParts {
            reader,
            writer,
            codec: self.codec,
            framer: self.framer,
            read_buf,
            unread,
            write_buf,
        }
    }

    /// Settles retained operations in both directions and recovers all components.
    pub async fn into_parts(mut self) -> SettledFramedParts<R, W, C, F, B> {
        let (read_result, write_result) = join(settle_read(&mut self.read), settle_write(&mut self.write)).await;
        SettledFramedParts {
            parts: self.into_parts_unchecked(),
            read_result: read_result.transpose().map(|_| ()),
            write_result: write_result.unwrap_or(Ok(())),
        }
    }
}

impl<R, W, C, F, B> Stream for Framed<R, W, C, F, B>
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

impl<R, W, C, F, B, Item> Sink<Item> for Framed<R, W, C, F, B>
where
    W: AsyncWrite + 'static,
    C: Encoder<Item, B>,
    F: Framer<B>,
    B: IoBufMut + IoBufMutExt + 'static,
{
    type Error = C::Error;

    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        send_item(&mut self.write, &mut self.codec, &mut self.framer, item).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        flush_write(&mut self.write).await.map_err(Into::into)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        close_write(&mut self.write).await.map_err(Into::into)
    }
}

async fn join<A, B>(left: A, right: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    let mut left_output = None;
    let mut right_output = None;

    std::future::poll_fn(|context| {
        if left_output.is_none()
            && let Poll::Ready(output) = left.as_mut().poll(context)
        {
            left_output = Some(output);
        }
        if right_output.is_none()
            && let Poll::Ready(output) = right.as_mut().poll(context)
        {
            right_output = Some(output);
        }
        match (left_output.take(), right_output.take()) {
            (Some(left), Some(right)) => Poll::Ready((left, right)),
            (left, right) => {
                left_output = left;
                right_output = right;
                Poll::Pending
            }
        }
    })
    .await
}
