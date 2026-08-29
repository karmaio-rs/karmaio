//! Runtime, task spawning, blocking work, and opt-in I/O cancellation.
//!
//! # I/O cancellation
//!
//! Wrap an ordinary I/O future with [`FutureExt::with_cancellation`]. A source
//! owns cancellation authority; its copyable tokens only observe cancellation
//! and register operations:
//!
//! ```ignore
//! use karmaio::io::AsyncRead;
//! use karmaio::runtime::{CancellationSource, FutureExt};
//!
//! let source = CancellationSource::new();
//! let token = source.token();
//! karmaio::runtime::spawn_local(async move {
//!     source.cancel();
//! });
//! let (res, buf) = stream.read(buf).with_cancellation(token).await;
//! ```
//!
//! [`CancellationSource::cancel`] is sticky and non-blocking. The observing
//! future must still be awaited; a completion that races cancellation may win
//! and return `Ok`. Dropping an I/O future requests platform cancellation and
//! forfeits the buffer.
//!
//! To time out an operation and recover its buffer, keep the wrapped future and
//! await it after requesting cancellation:
//!
//! ```ignore
//! use std::pin::pin;
//! use karmaio::io::AsyncRead;
//! use karmaio::runtime::{CancellationSource, FutureExt};
//! use karmaio::time::sleep;
//!
//! let source = CancellationSource::new();
//! let mut op = pin!(stream.read(buf).with_cancellation(source.token()));
//! let result = karmaio::select! {
//!     result = &mut op => result,
//!     _ = sleep(duration) => {
//!         source.cancel();
//!         op.await
//!     }
//! };
//! ```
//!
//! Nested `.with_cancellation` combinators all apply; cancelling either source
//! cancels the submitted operation.
//!
//! ## Scope and submission
//!
//! A cancellation combinator affects karmaio I/O operations submitted while
//! its inner future is being polled. Bind it before the operation's first poll:
//! once an operation has been submitted, wrapping later does not attach that
//! existing operation to the token. Likewise, wrap a multishot stream before
//! its first [`crate::io::Stream::next`] call.
//!
//! The combinator does not make arbitrary futures cancellation-aware. For
//! example, wrapping [`std::future::pending`] does not make it complete when
//! the source is cancelled. Use [`CancellationToken::cancelled`] when a
//! non-I/O future needs to observe cancellation cooperatively.
//!
//! Cancellation scopes also do not propagate into tasks created with
//! [`spawn_local`]. A spawned task is polled independently, so pass it a token
//! explicitly and wrap the karmaio I/O operations inside that task.
//!
//! ## Stream ordering
//!
//! Platform cancellation is best-effort. If a stream read is dropped without
//! awaiting its terminal completion, starting another read on the same stream
//! can race the abandoned read. Cancel and then await the original future when
//! operation ordering matters.

use crate::task::Task;

pub mod blocking;
mod cancel;
pub mod local;

pub use crate::io::StreamExt;
pub use crate::task::{JoinError, JoinHandle};
pub use blocking::{BlockingPool, BlockingPoolHandle};
pub(crate) use cancel::map_cancel_result;
pub use cancel::{
    CancellationSource, CancellationToken, FutureExt, OperationCanceled, WithCancellation, is_operation_canceled,
    operation_canceled,
};
pub use local::{Runtime, spawn_blocking, spawn_local};

pub(crate) trait Schedule: Send + Sync + Sized + 'static {
    /// Schedule the task
    fn schedule(&self, task: Task<Self>);

    /// Schedule the task to run in the near future, yielding the thread to
    /// other tasks.
    fn yield_now(&self, task: Task<Self>) {
        self.schedule(task);
    }

    /// Polling the task resulted in a panic. Should the runtime shutdown?
    fn unhandled_panic(&self) {
        // By default, do nothing
    }
}
