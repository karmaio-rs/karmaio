use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(not(target_os = "linux"))]
use std::{
    any::Any,
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::driver::Handle;
use crate::driver::backends::Operation;
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::UringMultishotOperation;
#[cfg(target_os = "linux")]
use crate::io::Stream;

// Generational op identities live in their own module; re-export so op and
// backend call sites can keep using `crate::driver::ops::{OpKey, OpTable, ...}`.
pub(crate) use crate::driver::op_table::{OpKey, OpTable};
// The IOCP backend decodes completions from raw pointers instead of reserved
// control tokens, so it never needs the completion key type.
#[cfg(not(target_os = "windows"))]
pub(crate) use crate::driver::op_table::CompletionKey;

// Always available: every `SharedIoHandle<T>` path closes through the driver.
pub(crate) mod close;

// Filesystem ops (`feature = "fs"`).
#[cfg(feature = "fs")]
pub(crate) mod create_dir;
#[cfg(feature = "fs")]
pub(crate) mod hardlink;
#[cfg(feature = "fs")]
pub(crate) mod open;
#[cfg(all(feature = "fs", unix))]
pub(crate) mod path_stat;
#[cfg(feature = "fs")]
pub(crate) mod read_at;
// Vectored file I/O is only wired up on Unix and Linux; on Windows the
// scatter/gather syscalls require page-aligned segments that the generic
// `IoVectoredBuf` API cannot guarantee.
#[cfg(all(feature = "fs", not(target_os = "windows")))]
pub(crate) mod readv;
#[cfg(feature = "fs")]
pub(crate) mod rename;
#[cfg(all(feature = "fs", not(target_os = "linux")))]
pub(crate) mod set_permissions;
#[cfg(feature = "fs")]
pub(crate) mod stat;
#[cfg(feature = "fs")]
pub(crate) mod symlink;
#[cfg(feature = "fs")]
pub(crate) mod sync;
#[cfg(feature = "fs")]
pub(crate) mod truncate;
#[cfg(feature = "fs")]
pub(crate) mod unlink;
#[cfg(feature = "fs")]
pub(crate) mod write_at;
#[cfg(all(feature = "fs", not(target_os = "windows")))]
pub(crate) mod writev;

// Network ops (`feature = "net"`).
#[cfg(feature = "net")]
pub(crate) mod accept;
#[cfg(all(feature = "net", target_os = "linux"))]
pub(crate) mod accept_multi;
#[cfg(feature = "net")]
pub(crate) mod connect;
#[cfg(feature = "net")]
pub(crate) mod recv;
#[cfg(feature = "net")]
pub(crate) mod recv_from;
#[cfg(all(feature = "net", target_os = "linux"))]
pub(crate) mod recv_from_managed;
#[cfg(all(feature = "net", target_os = "linux"))]
pub(crate) mod recv_from_multi;
#[cfg(all(feature = "net", target_os = "linux"))]
pub(crate) mod recv_managed;
#[cfg(all(feature = "net", target_os = "linux"))]
pub(crate) mod recv_multi;
#[cfg(feature = "net")]
pub(crate) mod recvmsg;
#[cfg(feature = "net")]
pub(crate) mod send;
#[cfg(feature = "net")]
pub(crate) mod send_to;
#[cfg(feature = "net")]
pub(crate) mod sendmsg;

// Process / pipe stream ops (`feature = "process"`).
// Offset-less read/write are used by child stdio pipes (not seekable).
#[cfg(feature = "process")]
pub(crate) mod read;
#[cfg(all(feature = "process", target_os = "linux"))]
pub(crate) mod wait_process;
#[cfg(feature = "process")]
pub(crate) mod write;

/// Terminal result shared by the backend protocols.
///
/// `result` is the portable syscall/completion outcome. `flags` preserves
/// backend completion metadata (for example io_uring CQE flags) for future
/// multishot and metadata-aware ops without forcing every caller to know the
/// source backend.
pub(crate) struct Completion {
    pub(crate) result: io::Result<u32>,
    #[allow(dead_code)] // Reserved for multishot / CQE metadata consumers.
    pub(crate) flags: u32,
    /// Owned result produced by a blocking syscall when it cannot be represented
    /// by the portable scalar completion value.
    #[cfg(not(target_os = "linux"))]
    blocking_value: Option<Box<dyn Any + Send>>,
}

impl Completion {
    /// Construct a terminal completion with no backend metadata flags.
    #[inline]
    pub(crate) fn new(result: io::Result<u32>) -> Self {
        Self {
            result,
            flags: 0,
            #[cfg(not(target_os = "linux"))]
            blocking_value: None,
        }
    }

    /// Construct a terminal completion that carries backend metadata flags.
    #[inline]
    #[allow(dead_code)] // Used by backends that surface CQE/completion flags.
    pub(crate) fn with_flags(result: io::Result<u32>, flags: u32) -> Self {
        Self {
            result,
            flags,
            #[cfg(not(target_os = "linux"))]
            blocking_value: None,
        }
    }

    /// Construct a completion from a blocking operation with a typed result.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn from_blocking_result<T: Send + 'static>(result: io::Result<T>) -> Self {
        match result {
            Ok(value) => Self {
                result: Ok(0),
                flags: 0,
                blocking_value: Some(Box::new(value)),
            },
            Err(error) => Self::new(Err(error)),
        }
    }

    /// Recover the typed value produced by a successful blocking operation.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn into_blocking_value<T: Send + 'static>(mut self) -> io::Result<T> {
        self.result?;
        let value = self
            .blocking_value
            .take()
            .unwrap_or_else(|| panic!("blocking completion missing {}", std::any::type_name::<T>()));
        Ok(value
            .downcast::<T>()
            .map(|value| *value)
            .unwrap_or_else(|_| panic!("blocking completion type mismatch for {}", std::any::type_name::<T>())))
    }

    /// Validate a byte-count completion against the submitted buffer capacity.
    ///
    /// Returns an error when the platform reports more bytes than the buffer
    /// could hold, which would otherwise make `set_init` unsound.
    pub(crate) fn bytes_transferred(self, capacity: usize) -> io::Result<usize> {
        let n = self.result? as usize;
        if n > capacity {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("operation returned more than the submitted buffer capacity ({capacity} bytes)"),
            ))
        } else {
            Ok(n)
        }
    }
}

/// Work deferred until the driver releases its `RefCell` borrow of the backend.
///
/// Completing an operation or waking a task may run arbitrary user code, so it
/// must not happen while the backend is mutably borrowed through the driver.
pub(crate) struct DeferredAction(Option<Box<dyn FnOnce() + 'static>>);

impl DeferredAction {
    pub(crate) fn new(action: impl FnOnce() + 'static) -> Self {
        Self(Some(Box::new(action)))
    }

    pub(crate) fn run(mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }

    pub(crate) fn run_all(actions: Vec<Self>) {
        for action in actions {
            action.run();
        }
    }
}

/// Completion queue shared by a readiness/completion backend and its blocking
/// workers.
#[cfg(not(target_os = "linux"))]
pub(crate) type BlockingCompletionQueue = Arc<Mutex<VecDeque<(OpKey, Completion)>>>;

/// Delivers exactly one terminal completion for a dispatched blocking job.
///
/// The blocking pool may discard a queued closure during shutdown. Keeping the
/// notifier in a guard ensures that such a job still retires its backend slot
/// and runs detached cleanup. The same guard also converts a worker panic into
/// a terminal completion when the worker closure unwinds.
#[cfg(not(target_os = "linux"))]
pub(crate) struct BlockingCompletionGuard {
    notifier: Option<BlockingCompletionNotifier>,
}

#[cfg(not(target_os = "linux"))]
struct BlockingCompletionNotifier {
    key: OpKey,
    done: BlockingCompletionQueue,
    wakeup: crate::driver::Wakeup,
}

#[cfg(not(target_os = "linux"))]
impl BlockingCompletionGuard {
    pub(crate) fn new(key: OpKey, done: BlockingCompletionQueue, wakeup: crate::driver::Wakeup) -> Self {
        Self {
            notifier: Some(BlockingCompletionNotifier { key, done, wakeup }),
        }
    }

    /// Deliver the completion produced by a normally returning job.
    pub(crate) fn complete(mut self, completion: Completion) {
        self.send(completion);
    }

    fn send(&mut self, completion: Completion) {
        let Some(notifier) = self.notifier.take() else {
            return;
        };

        notifier
            .done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back((notifier.key, completion));
        notifier.wakeup.wake();
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for BlockingCompletionGuard {
    fn drop(&mut self) {
        self.send(Completion::new(Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "blocking operation cancelled before completion",
        ))));
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod blocking_completion_tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn dropped_guard_delivers_one_interrupted_completion() {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakeup = crate::driver::Wakeup::new({
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        let key = OpTable::new(1).unwrap().insert(()).unwrap();

        drop(BlockingCompletionGuard::new(key, Arc::clone(&queue), wakeup));

        let completions = queue.lock().unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].0, key);
        assert_eq!(
            completions[0].1.result.as_ref().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn completed_guard_does_not_notify_again_on_drop() {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakeup = crate::driver::Wakeup::new({
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        let key = OpTable::new(1).unwrap().insert(()).unwrap();
        let guard = BlockingCompletionGuard::new(key, Arc::clone(&queue), wakeup);

        guard.complete(Completion::new(Ok(7)));

        let completions = queue.lock().unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].1.result.as_ref().unwrap(), &7);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }
}

/// A typed one-shot operation future shared by all backends.
///
/// The future owns the logical operation payload in a stable heap allocation.
/// The selected backend owns only the lifecycle slot and any platform-specific
/// submission state, and drives this future through the target-local
/// `Operation` protocol.
///
/// Keeping the payload boxed is required for operations whose native control
/// data points into the operation itself, including buffers with inline
/// storage. Moving the `Op` future only moves the box pointer.
pub(crate) struct Op<T: Operation + 'static> {
    driver: Handle,
    key: OpKey,
    data: Option<Box<T>>,
}

impl<T: Operation + 'static> Op<T> {
    pub(crate) fn new(key: OpKey, data: Box<T>, driver: Handle) -> Self {
        Self {
            driver,
            key,
            data: Some(data),
        }
    }

    pub(crate) fn key(&self) -> OpKey {
        self.key
    }

    pub(crate) fn take_data(&mut self) -> Option<Box<T>> {
        self.data.take()
    }

    #[allow(dead_code)]
    pub(crate) fn data_ref(&self) -> Option<&T> {
        self.data.as_deref()
    }

    #[allow(dead_code)]
    pub(crate) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_deref_mut()
    }
}

impl<T: Operation + 'static> Future for Op<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver
            .upgrade()
            .expect("Not in runtime context")
            .poll_op(self.get_mut(), cx)
    }
}

impl<T: Operation + 'static> Drop for Op<T> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.remove_op(self);
        }
    }
}

/// A typed multishot operation that yields zero or more items as a stream.
///
/// Multishot ops submit one SQE that may produce many CQEs. Intermediate CQEs
/// carry `IORING_CQE_F_MORE`; the final CQE does not. Dropping the stream
/// cancels the in-flight request (unlike oneshot [`Op`], which detaches).
///
/// Multishot APIs require Linux 6.12+. karmaio does not probe the kernel
/// version at runtime; callers must meet that floor.
///
/// Completions are staged on the driver and converted via
/// [`UringMultishotOperation::complete_item`] when polled.
#[cfg(target_os = "linux")]
pub(crate) struct MultiOp<T: UringMultishotOperation + 'static> {
    driver: Handle,
    key: OpKey,
    state: MultiOpState<T>,
}

#[cfg(target_os = "linux")]
enum MultiOpState<T> {
    Active(Box<T>),
    Terminated,
}

#[cfg(target_os = "linux")]
impl<T: UringMultishotOperation + 'static> MultiOp<T> {
    pub(crate) fn new(key: OpKey, data: Box<T>, driver: Handle) -> Self {
        Self {
            driver,
            key,
            state: MultiOpState::Active(data),
        }
    }

    pub(crate) fn key(&self) -> OpKey {
        self.key
    }

    pub(crate) fn take_data(&mut self) -> Option<Box<T>> {
        match std::mem::replace(&mut self.state, MultiOpState::Terminated) {
            MultiOpState::Active(data) => Some(data),
            MultiOpState::Terminated => None,
        }
    }

    pub(crate) fn data_mut(&mut self) -> Option<&mut T> {
        match &mut self.state {
            MultiOpState::Active(data) => Some(data),
            MultiOpState::Terminated => None,
        }
    }

    fn is_terminated(&self) -> bool {
        matches!(self.state, MultiOpState::Terminated)
    }

    fn finish(&mut self) {
        self.state = MultiOpState::Terminated;
    }
}

#[cfg(target_os = "linux")]
impl<T: UringMultishotOperation + 'static> Stream for MultiOp<T> {
    type Item = T::Item;

    async fn next(&mut self) -> Option<Self::Item> {
        MultiOpNext { op: self }.await
    }
}

/// One poll step of [`MultiOp::next`].
#[cfg(target_os = "linux")]
struct MultiOpNext<'a, T: UringMultishotOperation + 'static> {
    op: &'a mut MultiOp<T>,
}

#[cfg(target_os = "linux")]
impl<T: UringMultishotOperation + 'static> Future for MultiOpNext<'_, T> {
    type Output = Option<T::Item>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.op.is_terminated() {
            return Poll::Ready(None);
        }
        let Some(driver) = this.op.driver.upgrade() else {
            this.op.finish();
            return Poll::Ready(None);
        };
        match driver.poll_multi_op(this.op, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.op.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Some(completion)) => {
                let data = this.op.data_mut().expect("multishot op data missing while streaming");
                let item = UringMultishotOperation::complete_item(data, completion);
                if item.is_none() {
                    // `None` is operation-level termination. Retire or cancel
                    // the backend slot before making the stream terminal.
                    driver.remove_multi_op(this.op);
                }
                Poll::Ready(item)
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl<T: UringMultishotOperation + 'static> Drop for MultiOp<T> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.remove_multi_op(self);
        }
    }
}

/// Send-able unit of work for the blocking thread pool.
///
/// Built inside a readiness backend's submission attempt when an operation
/// must run on the blocking pool.
/// Captures only `Send` state (paths, raw fds, flags) so the runtime thread can
/// keep non-`Send` op data (e.g. `SharedIoHandle`) while the syscall runs off-thread.
/// Used on kqueue Unix targets / Windows; io_uring handles equivalent work in-kernel.
///
/// [`BlockingJob::run`] retries `io::ErrorKind::Interrupted` so individual
/// callers never have to re-enter the readiness state machine for EINTR.
#[allow(dead_code)]
pub(crate) struct BlockingJob {
    work: Box<dyn FnMut() -> Completion + Send + 'static>,
}

#[allow(dead_code)] // Used on macOS / Windows; unused on pure io_uring Linux builds.
impl BlockingJob {
    pub(crate) fn new(work: impl FnMut() -> Completion + Send + 'static) -> Self {
        Self { work: Box::new(work) }
    }

    /// Run the job, retrying only `Interrupted` results.
    pub(crate) fn run(mut self) -> Completion {
        loop {
            let completion = (self.work)();
            if !matches!(&completion.result, Err(error) if error.kind() == io::ErrorKind::Interrupted) {
                return completion;
            }
        }
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod blocking_job_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn run_retries_interrupted_results() {
        use std::sync::Arc;

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_job = Arc::clone(&attempts);
        let job = BlockingJob::new(move || {
            let n = attempts_job.fetch_add(1, Ordering::Relaxed);
            if n < 2 {
                Completion::new(Err(io::Error::new(io::ErrorKind::Interrupted, "eintr")))
            } else {
                Completion::new(Ok(9))
            }
        });

        let completion = job.run();
        assert_eq!(completion.result.unwrap(), 9);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }
}
