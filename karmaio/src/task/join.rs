use std::{
    any::Any,
    fmt,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::task::raw::RawTask;

/// Result returned by a task join handle.
pub type Result<T> = std::result::Result<T, JoinError>;

/// Error returned when a task does not complete successfully.
pub struct JoinError {
    kind: JoinErrorKind,
}

enum JoinErrorKind {
    Cancelled,
    Panic(Box<dyn Any + Send + 'static>),
}

impl JoinError {
    pub(crate) fn cancelled() -> Self {
        Self {
            kind: JoinErrorKind::Cancelled,
        }
    }

    pub(crate) fn panic(payload: Box<dyn Any + Send + 'static>) -> Self {
        Self {
            kind: JoinErrorKind::Panic(payload),
        }
    }

    /// Returns `true` if the task was cancelled before completing.
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, JoinErrorKind::Cancelled)
    }

    /// Returns `true` if the task panicked while being polled.
    pub fn is_panic(&self) -> bool {
        matches!(self.kind, JoinErrorKind::Panic(_))
    }

    /// Consumes the error, returning the panic payload if the task panicked.
    ///
    /// # Panics
    ///
    /// Panics if the error was caused by task cancellation.
    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        match self.kind {
            JoinErrorKind::Panic(payload) => payload,
            JoinErrorKind::Cancelled => panic!("JoinError is not a panic"),
        }
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            JoinErrorKind::Cancelled => "Cancelled",
            JoinErrorKind::Panic(_) => "Panic",
        };

        f.debug_struct("JoinError").field("kind", &kind).finish()
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            JoinErrorKind::Cancelled => f.write_str("task was cancelled"),
            JoinErrorKind::Panic(_) => f.write_str("task panicked"),
        }
    }
}

impl std::error::Error for JoinError {}

/// Handle for awaiting (or aborting) a spawned task.
///
/// # Detach vs abort
///
/// Dropping a `JoinHandle` **detaches** from the task: the task keeps running
/// and is **not** cancelled. Use [`JoinHandle::abort`] for cooperative
/// cancellation. This matches the usual Tokio-style join-handle contract.
///
/// # Runtime lifetime
///
/// The handle is only useful while its
/// [`Runtime`](crate::runtime::Runtime) is still alive and driving the
/// scheduler. Awaiting after the runtime has been dropped will hang. Prefer
/// finishing or aborting work before dropping the runtime (see
/// [Runtime shutdown](crate::runtime::Runtime#shutdown)).
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}

// SAFETY: `JoinHandle<T>` only yields `T` (or a `JoinError`) once the task has
// completed on the runtime thread. The handle itself only carries the right to
// observe or cancel the result. Therefore it is `Send`/`Sync` precisely when `T` is.
unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Sync> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    pub(super) fn new(raw: RawTask) -> JoinHandle<T> {
        JoinHandle { raw, _p: PhantomData }
    }
    pub fn is_finished(&self) -> bool {
        let state = self.raw.header().state.get_snapshot();
        state.is_complete()
    }

    /// Requests cancellation of the task.
    ///
    /// Cancellation is cooperative with the executor: if the task is currently
    /// running, it will be completed with a cancellation error after the
    /// current poll returns.
    pub fn abort(&self) {
        self.raw.cancel();
    }
}

impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut ret = Poll::Pending;

        // Try to read the task output. If the task is not yet complete, the
        // waker is stored and is notified once the task does complete.
        //
        // The function must go via the vtable, which requires erasing generic
        // types. To do this, the function "return" is placed on the stack
        // **before** calling the function and is passed into the function using
        // `*mut ()`.
        //
        // Safety:
        //
        // The type of `T` must match the task's output type.

        self.raw.try_read_output(&mut ret as *mut _ as *mut (), cx.waker());

        ret
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if self.raw.header().state.drop_join_handle_fast().is_ok() {
            return;
        }

        self.raw.drop_join_handle();
    }
}
