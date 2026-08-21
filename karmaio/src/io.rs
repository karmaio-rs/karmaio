//! Completion-based asynchronous I/O traits and utilities.
//!
//! Operations take ownership of their buffers and return them on completion,
//! allowing platform drivers to retain stable memory across asynchronous I/O.

pub(crate) mod cancel;
mod framed;
mod read;
mod sink;
mod stream;
mod write;

pub use cancel::{CancelHandle, Canceller, OperationCanceled, is_operation_canceled, operation_canceled};
pub(crate) use cancel::{Register, TerminalGuard, map_cancel_result};
pub use framed::{
    AnyDelimited, BytesCodec, CharDelimited, Decoder, Encoder, Frame, Framed, FramedRead, FramedWrite, Framer,
    LengthDelimited, LineDelimited, NoopFramer,
};
pub use read::{
    AsyncBufRead, AsyncRead, AsyncReadAt, AsyncReadAtExt, AsyncReadCancellable, AsyncReadExt, AsyncReadManaged,
    AsyncReadMulti, BufReader,
};
pub use sink::{Sink, SinkExt};
pub use stream::{Stream, StreamExt};
pub use write::{AsyncWrite, AsyncWriteAt, AsyncWriteAtExt, AsyncWriteCancellable, AsyncWriteExt, BufWriter};
