//! Completion-based asynchronous I/O traits and utilities.
//!
//! Operations take ownership of their buffers and return them on completion,
//! allowing platform drivers to retain stable memory across asynchronous I/O.
//! Ordinary I/O methods remain the only operation verbs. Opt into eager
//! cancellation by wrapping the future that submits the operation with
//! [`crate::runtime::FutureExt::with_cancellation`] and a token from
//! [`crate::runtime::CancellationSource`].
//! Wrap before the operation's first poll; cancellation scopes affect karmaio
//! I/O submissions, not arbitrary futures or independently spawned tasks.

mod framed;
mod read;
mod sink;
mod split;
mod stream;
mod write;

pub use framed::{
    AnyDelimited, BytesCodec, CharDelimited, Decoder, Encoder, Frame, Framed, FramedParts, FramedRead, FramedReadParts,
    FramedWrite, FramedWriteParts, Framer, LengthDelimited, LineDelimited, NoopFramer, SettledFramedParts,
    SettledFramedReadParts, SettledFramedWriteParts,
};
pub use read::{
    AsyncBufRead, AsyncRead, AsyncReadAt, AsyncReadAtExt, AsyncReadExt, AsyncReadManaged, AsyncReadMulti, BufReader,
};
pub use sink::{Sink, SinkExt};
pub use split::{IntoOwnedSplit, ReuniteError, ReuniteErrorKind, ReuniteOwned};
pub use stream::{Stream, StreamExt};
pub use write::{AsyncWrite, AsyncWriteAt, AsyncWriteAtExt, AsyncWriteExt, BufWriter};
