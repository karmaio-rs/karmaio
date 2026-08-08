//! Completion-based asynchronous I/O traits and utilities.
//!
//! Operations take ownership of their buffers and return them on completion,
//! allowing platform drivers to retain stable memory across asynchronous I/O.

mod framed;
mod read;
mod sink;
mod stream;
mod write;

pub use framed::{
    AnyDelimited, BytesCodec, CharDelimited, Decoder, Encoder, Frame, Framed, FramedRead, FramedWrite, Framer,
    LengthDelimited, LineDelimited, NoopFramer,
};
pub use read::{AsyncBufRead, AsyncRead, AsyncReadAt, AsyncReadAtExt, AsyncReadExt, BufReader};
pub use sink::{Sink, SinkExt};
pub use stream::{Stream, StreamExt};
pub use write::{AsyncWrite, AsyncWriteAt, AsyncWriteAtExt, AsyncWriteExt, BufWriter};
