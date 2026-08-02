use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(not(target_os = "linux"))]
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::driver::Handle;
use crate::driver::backends::Operation;

// Generational op identities live in their own module; re-export so op and
// backend call sites can keep using `crate::driver::ops::{OpKey, OpTable, ...}`.
pub(crate) use crate::driver::op_table::{CompletionKey, OpKey, OpTable};

// Always available: every `SharedIoHandle<T>` path closes through the driver.
pub(crate) mod close;

// Filesystem ops (`feature = "fs"`).
#[cfg(feature = "fs")]
pub(crate) mod create_dir;
#[cfg(feature = "fs")]
pub(crate) mod hardlink;
#[cfg(feature = "fs")]
pub(crate) mod open;
#[cfg(feature = "fs")]
pub(crate) mod read_at;
#[cfg(feature = "fs")]
pub(crate) mod readv;
#[cfg(feature = "fs")]
pub(crate) mod rename;
#[cfg(feature = "fs")]
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
#[cfg(feature = "fs")]
pub(crate) mod writev;

// Network ops (`feature = "net"`).
#[cfg(feature = "net")]
pub(crate) mod accept;
#[cfg(feature = "net")]
pub(crate) mod connect;
#[cfg(feature = "net")]
pub(crate) mod recv;
#[cfg(feature = "net")]
pub(crate) mod recv_from;
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
}

impl Completion {
    /// Construct a terminal completion with no backend metadata flags.
    #[inline]
    pub(crate) fn new(result: io::Result<u32>) -> Self {
        Self { result, flags: 0 }
    }

    /// Construct a terminal completion that carries backend metadata flags.
    #[inline]
    #[allow(dead_code)] // Used by backends that surface CQE/completion flags.
    pub(crate) fn with_flags(result: io::Result<u32>, flags: u32) -> Self {
        Self { result, flags }
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
            "blocking operation cancelled before completion"))));
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

        guard.complete(Completion::new(Ok(7) ));

        let completions = queue.lock().unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].1.result.as_ref().unwrap(), &7);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }
}

/// A typed one-shot operation future shared by all backends.
///
/// The future owns the logical operation payload. The selected backend owns
/// only the lifecycle slot and any platform-specific submission state, and
/// drives this future through the target-local `Operation` protocol.
pub(crate) struct Op<T: Operation + 'static> {
    driver: Handle,
    key: OpKey,
    data: Option<T>,
}

impl<T: Operation + 'static> Op<T> {
    pub(crate) fn new(key: OpKey, data: T, driver: Handle) -> Self {
        Self {
            driver,
            key,
            data: Some(data),
        }
    }

    pub(crate) fn key(&self) -> OpKey {
        self.key
    }

    pub(crate) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    #[allow(dead_code)]
    pub(crate) fn data_ref(&self) -> Option<&T> {
        self.data.as_ref()
    }

    #[allow(dead_code)]
    pub(crate) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }
}

impl<T: Operation + Unpin + 'static> Future for Op<T> {
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
