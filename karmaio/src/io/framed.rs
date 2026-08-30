//! Framed and streaming I/O adapters for completion-based transports.
//!
//! This module layers framing and codecs on top of [`crate::io::AsyncRead`] /
//! [`crate::io::AsyncWrite`], using owned buffers suitable for io_uring / IOCP /
//! kqueue completion models.
//!
//! # Overview
//!
//! - [`crate::io::Stream`] / [`crate::io::Sink`]: pure-async stream and sink traits
//! - [`Framer`]: byte-level frame layout (length prefix, delimiters, …)
//! - [`Encoder`] / [`Decoder`]: payload ↔ typed messages
//! - [`FramedRead`] / [`FramedWrite`] / [`Framed`]: combined adapters
//!
//! The buffer type parameter defaults to `Vec<u8>` but is generic over
//! [`crate::buf::IoBufMut`] + [`crate::buf::IoBufMutExt`] so future buffer types
//! (e.g. `bytes::BytesMut`) can plug in without changing codec APIs.

mod buffer;
mod codec;
mod frame;
mod framed_read;
mod framed_rw;
mod framed_write;

pub use codec::{BytesCodec, Decoder, Encoder};
pub use frame::{AnyDelimited, CharDelimited, Frame, Framer, LengthDelimited, LineDelimited, NoopFramer};
pub use framed_read::{FramedRead, FramedReadParts};
pub use framed_rw::{Framed, FramedParts};
pub use framed_write::{FramedWrite, FramedWriteParts};
