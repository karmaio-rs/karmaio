//! Completion-native TLS streams backed by Rustls.
//!
//! Enable the `tls` convenience feature, or select `tls-ring` or
//! `tls-aws-lc-rs` explicitly. TLS does not establish a network connection or
//! choose certificate roots; callers provide both a transport and a configured
//! Rustls client or server configuration.
//!
//! Each stream owns one bidirectional Rustls connection. When the transport
//! implements [`IntoOwnedSplit`](crate::io::IntoOwnedSplit), the TLS stream can
//! be split into owned halves that make independent transport progress. Split
//! halves share only synchronous protocol state on the local runtime; neither
//! half holds a Rustls borrow while awaiting transport I/O. They remain generic
//! over the transport and do not require the `net` feature. The adapter does not
//! implement managed or multishot reads.
//!
//! Rustls can generate control output while processing input. An unsplit stream
//! preserves eager behavior and sends that output before its read completes.
//! A split read never drives the transport writer: it leaves KeyUpdate responses
//! and similar output in Rustls's internal buffer. The next write, flush, or
//! shutdown sends it first. A fatal protocol error is returned immediately by
//! the reader; the next writer operation attempts the queued alert and then
//! returns the same terminal error class.
//!
//! Writes are write-through: success means every TLS record generated for the
//! accepted plaintext reached the wrapped [`AsyncWrite`](crate::io::AsyncWrite).
//! Vectored writes feed caller components directly into Rustls, and vectored
//! reads distribute decrypted plaintext directly across caller components in
//! order. Adapter-owned ciphertext queues use fixed-capacity allocations;
//! Rustls retains control of its own internal buffering policy.
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
//! Ordinary transport read errors fail only the read direction. TLS protocol
//! errors, cancellation, abandoned transport operations, and write, flush, or
//! shutdown errors fail both directions because wire progress can be ambiguous.
//! An opposite-direction operation already submitted may settle and recover its
//! buffer before observing the sticky terminal error. Dropping an established
//! I/O future while it awaits the transport also makes both directions terminal.
//!
//! [`AsyncWrite::shutdown`](crate::io::AsyncWrite::shutdown) on an owned write
//! half drains control output, sends `close_notify`, flushes, and shuts down the
//! transport write direction. Dropping it without shutdown is an abrupt TLS
//! close. Dropping either half does not otherwise invalidate the surviving
//! direction. Connection metadata and raw parts are intentionally unavailable
//! through split halves; inspect metadata before splitting or after reunion.
//!
//! `into_parts` is an abrupt escape hatch: it sends no `close_notify` and does
//! not shut down the transport.

mod buffer;
mod engine;
mod error;
mod split;

pub mod client;
pub mod server;

pub use client::{
    OwnedReadHalf as ClientTlsReadHalf, OwnedWriteHalf as ClientTlsWriteHalf, TlsConnector,
    TlsStream as ClientTlsStream,
};
pub use rustls;
pub use server::{
    OwnedReadHalf as ServerTlsReadHalf, OwnedWriteHalf as ServerTlsWriteHalf, TlsAcceptor, TlsStream as ServerTlsStream,
};
