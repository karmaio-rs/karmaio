//! Asynchronous signal handling.
//!
//! This module provides cross-platform notification of operating-system
//! signals:
//! - [`ctrl_c`] returns a future that completes when the process receives a
//!   Ctrl-C notification. It is available on every platform.
//! - On Unix, [`Signal`] and [`signal`] listen for arbitrary signals (for
//!   example `SIGTERM` or `SIGHUP`), and can be awaited repeatedly.
//!
//! # Examples
//!
//! Wait for Ctrl-C (cross-platform):
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! karmaio::signal::ctrl_c()?.await?;
//! println!("received ctrl-c");
//! # Ok(())
//! # }
//! ```
//!
//! Wait for `SIGTERM` (Unix only):
//!
//! ```no_run
//! # #[cfg(unix)]
//! # async fn run() -> std::io::Result<()> {
//! use karmaio::signal::{signal, SignalKind};
//!
//! let mut term = signal(SignalKind::terminate())?;
//! term.recv().await?;
//! println!("received SIGTERM");
//! # Ok(())
//! # }
//! ```
//!
//! # Implementation notes
//! - On Unix, an OS signal handler is installed lazily for each distinct signal
//!   and notifies every registered listener directly. The registry is read from
//!   the handler through a lock-free `half_lock` structure so it stays async-signal-safe.
//! - On Windows, Ctrl-C is handled via `SetConsoleCtrlHandler`.
//! - Signals coalesce: a listener that does not keep up with rapid deliveries
//!   observes a single notification, matching the behaviour of most runtimes.

#[cfg(unix)]
mod half_lock;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{CtrlC, Signal, SignalKind, ctrl_c, signal};
#[cfg(windows)]
pub use windows::{CtrlC, ctrl_c};
