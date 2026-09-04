//! Completion-native TLS streams backed by Rustls.
//!
//! Enable the `tls` convenience feature, or select `tls-ring` or
//! `tls-aws-lc-rs` explicitly. TLS does not establish a network connection or
//! choose certificate roots; callers provide both a transport and a configured
//! Rustls client or server configuration.
//!
//! Each stream owns one bidirectional Rustls connection. TLS reads can produce
//! protocol writes, so streams intentionally do not support borrowed or owned
//! splitting. The adapter also does not implement managed or multishot reads.
//!
//! Writes are write-through: success means every TLS record generated for the
//! accepted plaintext reached the wrapped [`AsyncWrite`](crate::io::AsyncWrite).
//! Scalar and vectored calls accept at most 16 KiB; vectored input is gathered
//! into fixed initialized staging. Vectored reads distribute up to 16 KiB of
//! decrypted plaintext across caller components in order. Rustls and adapter
//! queues are bounded.
//!
//! A peer `close_notify` becomes a clean `Ok(0)` after buffered plaintext is
//! delivered. A transport EOF without that alert becomes
//! [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof), preserving TLS
//! truncation detection. [`AsyncWrite::shutdown`](crate::io::AsyncWrite::shutdown)
//! sends `close_notify`, flushes it, and shuts down the transport write
//! direction without waiting for the peer's alert.
//!
//! Karmaio cancellation scopes flow into the wrapped transport operations.
//! Established reads and writes return the original caller buffer through
//! [`BufResult`](crate::buf::BufResult). Keep and await the same scoped I/O
//! future when preserving a caller buffer; do not discard it with a timeout.
//! Cancellation conservatively makes the TLS stream unusable because wire
//! progress can be ambiguous. Dropping an established I/O future while it is
//! awaiting the transport also makes the stream terminal, preventing reuse
//! after buffer ownership or wire progress has become ambiguous.
//!
//! `into_parts` is an abrupt escape hatch: it sends no `close_notify` and does
//! not shut down the transport.

mod buffer;
mod engine;
mod error;

pub mod client;
pub mod server;

pub use client::{TlsConnector, TlsStream as ClientTlsStream};
pub use rustls;
pub use server::{TlsAcceptor, TlsStream as ServerTlsStream};
