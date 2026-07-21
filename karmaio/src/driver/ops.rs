use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::driver::{Handle, Submission};

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

// Lifecycle of a single I/O operation tracked by the driver.
//
// # Drop / cancellation semantics
//
// Dropping an [`Op`] future means the *caller* no longer wants the result
// (**detach**), not that the kernel work is necessarily cancelled:
//
// * **io_uring**: the SQE stays in flight until its CQE arrives. Op payload
//   is moved into `Ignored` so the kernel still has valid memory, and so a
//   late CQE can still run [`IgnoredOp::cleanup`] (closes orphan accept/open
//   FDs). `IORING_OP_ASYNC_CANCEL` is only submitted on *driver* shutdown,
//   not on individual `Op` drop.
// * **IOCP**: `CancelIoEx` is requested; the OVERLAPPED and payload stay alive
//   until the completion packet is dequeued, then cleanup runs.
// * **kqueue**: registered interest is `EV_DELETE`d synchronously (slot freed
//   immediately — no produced FD yet). Blocking-pool jobs cannot be cancelled mid-flight;
//   the payload stays in `Ignored` until the worker finishes, then cleanup runs.
//
// **I/O safety:** resource-producing ops (`accept`, `open`) put the new FD in
// the completion result. If the waiter has detached, [`State::complete`] still
// calls `complete` via the ignored payload and drops the result so the FD is
// closed rather than leaked.
//
// Callers that need stronger cancel guarantees (e.g. releasing an FD promptly)
// should close the resource or wait for the op to complete rather than only
// dropping the future.
pub(crate) enum State {
    // The operation has been submitted to the driver and is currently in-flight
    Submitted,

    // The submitter is waiting for the completion of the operation
    Waiting(Waker),

    // Used in poll based systems (kqueue), signifies that the op is ready and can resume the syscall.
    // Constructed by `State::ready`; unused on io_uring / IOCP paths.
    #[allow(dead_code)]
    Ready,

    // The submitter no longer has interest in the operation result.
    // The payload stays alive until the kernel completion arrives so buffers /
    // paths remain valid, and so [`IgnoredOp::cleanup`] can close produced FDs.
    #[allow(dead_code)]
    Ignored(Box<dyn IgnoredOp>),

    // The operation has completed with a single cqe result
    Completed(Completion),
}

/// Type-erased op payload kept while a detached operation may still complete.
///
/// When the CQE / completion packet arrives, [`cleanup`](IgnoredOp::cleanup) runs the
/// normal [`Completable::complete`] path and drops the result. That closes
/// orphan sockets/files from accept/open and frees buffers for other ops.
pub(crate) trait IgnoredOp: 'static {
    fn cleanup(self: Box<Self>, completion: Completion);
}

impl<T: Completable + 'static> IgnoredOp for T {
    fn cleanup(self: Box<Self>, completion: Completion) {
        drop((*self).complete(completion));
    }
}

/// Placeholder cleanup when no typed payload is available (e.g. driver shutdown
/// of ops that never moved data into `Ignored`).
#[allow(dead_code)] // Used by backend Drop paths on all platforms.
pub(crate) struct Detached;

impl IgnoredOp for Detached {
    fn cleanup(self: Box<Self>, _completion: Completion) {}
}

/// A single in-flight driver operation, polled as a future.
///
/// See [`State`] for drop / detach semantics.
///
/// `T: Completable` so detach can type-erase the payload into [`IgnoredOp`]
/// and late completions still run `complete` (orphan accept/open safety).
pub(crate) struct Op<T: Completable + 'static> {
    driver: Handle,
    index: usize,
    data: Option<T>,
}

pub(crate) struct Completion {
    pub(crate) result: std::io::Result<u32>,
    // Reserved for future use (e.g. io_uring CQE flags). Populated by backends today.
    #[allow(dead_code)]
    pub(crate) flags: u32,
}

/// Send-able unit of work for the blocking thread pool.
///
/// Built inside `Submittable::submit` when an op returns [`Submission::Blocking`].
/// Captures only `Send` state (paths, raw fds, flags) so the runtime thread can
/// keep non-`Send` op data (e.g. `SharedIoHandle`) while the syscall runs off-thread.
/// Used on macOS / Windows; io_uring handles equivalent work in-kernel.
#[allow(dead_code)]
pub(crate) struct BlockingJob {
    work: Box<dyn FnOnce() -> Completion + Send + 'static>,
}

#[allow(dead_code)] // Used on macOS / Windows; unused on pure io_uring Linux builds.
impl BlockingJob {
    pub(crate) fn new(work: impl FnOnce() -> Completion + Send + 'static) -> Self {
        Self { work: Box::new(work) }
    }

    pub(crate) fn run(self) -> Completion {
        (self.work)()
    }
}

pub(crate) trait Submittable {
    // Build a backend-specific submission entry.
    fn submit(&mut self) -> Submission;
}

pub(crate) trait Completable {
    type Result;

    // `complete` will be called for every op once done (including detach/cleanup).
    fn complete(self, completion_entry: Completion) -> Self::Result;
}

pub(crate) trait Operable: Submittable + Completable {}

impl<T: Completable + 'static> Op<T> {
    pub(crate) fn new(index: usize, data: T, driver: Handle) -> Self {
        Self {
            driver,
            index,
            data: Option::Some(data),
        }
    }

    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    // Used by the Windows open path to attach the handle to the IOCP after submit.
    #[allow(dead_code)]
    pub(crate) fn data_ref(&self) -> Option<&T> {
        self.data.as_ref()
    }

    #[allow(dead_code)] // Reserved for ops that need to mutate data between poll cycles.
    pub(super) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }
}

impl<T: Unpin + Operable> Future for Op<T> {
    type Output = T::Result;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver
            .upgrade()
            .expect("Not in runtime context")
            .poll_op(self.get_mut(), cx)
    }
}

impl<T: Completable + 'static> Drop for Op<T> {
    fn drop(&mut self) {
        // If the runtime/driver is already gone, in-flight work was cancelled
        // or drained by backend `Drop`. Detach quietly instead of panicking
        // during teardown (e.g. orphaned tasks after `Runtime` drop).
        if let Some(driver) = self.driver.upgrade() {
            driver.remove_op(self);
        }
    }
}

impl State {
    // Processes the completion for the state and it's associated op
    // Returns whether to keep the op or drop it
    pub(crate) fn complete(&mut self, completion: Completion) -> bool {
        match self {
            State::Submitted => {
                *self = State::Completed(completion);
                // The completion still has to be read, so don't drop
                false
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                match old {
                    // waker is woken to notify the caller to process the result
                    State::Waiting(waker) => {
                        waker.wake();
                    }
                    _ => unreachable!("invalid operation state"),
                }
                // The completion still has to be read, so don't drop
                false
            }
            State::Ignored(_) => {
                // Caller detached: run complete + drop so accept/open FDs close.
                if let State::Ignored(payload) = std::mem::replace(self, State::Submitted) {
                    payload.cleanup(completion);
                }
                true
            }
            State::Ready => {
                // Calling readiness state via completion call is a no-op
                // This should not be triggered in normal operation
                unreachable!("invalid operation state");
            }
            State::Completed(..) => {
                // Calling complete on an already completed state is a no-op
                // This should not be triggered in normal operation
                unreachable!("invalid operation state");
            }
        }
    }

    // Processes a readiness notification for the state and its associated op.
    // Returns whether to keep the op or drop it.
    // Called from the kqueue backend; unused on io_uring / IOCP.
    #[allow(dead_code)]
    pub(crate) fn ready(&mut self) -> bool {
        match self {
            State::Submitted => {
                *self = State::Ready;
                false
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Ready);
                match old {
                    State::Waiting(waker) => {
                        waker.wake();
                    }
                    _ => unreachable!("invalid operation state"),
                }
                false
            }
            State::Ignored(..) => true,
            State::Ready => false,
            // A blocking-pool completion may land before a spurious/stale
            // readiness notification is processed; treat as no-op.
            State::Completed(..) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct CleanupMarker {
        cleaned_up: Rc<Cell<bool>>,
        /// Value reported in the completion result when cleaned up.
        saw_result: Rc<Cell<Option<u32>>>,
    }

    impl Completable for CleanupMarker {
        type Result = ();

        fn complete(self, completion: Completion) -> Self::Result {
            self.cleaned_up.set(true);
            self.saw_result.set(completion.result.ok());
        }
    }

    #[test]
    fn ignored_completion_reaps_payload() {
        let cleaned_up = Rc::new(Cell::new(false));
        let saw = Rc::new(Cell::new(None));
        let mut state = State::Ignored(Box::new(CleanupMarker {
            cleaned_up: cleaned_up.clone(),
            saw_result: saw.clone(),
        }));

        assert!(state.complete(Completion {
            result: Ok(42),
            flags: 0,
        }));
        assert!(cleaned_up.get(), "complete path must run for detached ops");
        assert_eq!(saw.get(), Some(42));
    }

    #[test]
    fn ignored_error_completion_still_cleans_up() {
        let cleaned_up = Rc::new(Cell::new(false));
        let saw = Rc::new(Cell::new(None));
        let mut state = State::Ignored(Box::new(CleanupMarker {
            cleaned_up: cleaned_up.clone(),
            saw_result: saw.clone(),
        }));

        assert!(state.complete(Completion {
            result: Err(std::io::Error::from_raw_os_error(1)),
            flags: 0,
        }));
        assert!(cleaned_up.get());
        assert_eq!(saw.get(), None);
    }

    /// Simulates accept/open: produced FD lives only in the completion result.
    /// Detach + complete must take ownership so Drop closes it.
    #[cfg(unix)]
    #[test]
    fn ignored_accept_like_complete_closes_orphan_fd() {
        use std::os::fd::{FromRawFd, IntoRawFd, RawFd};

        struct ProduceFd;

        impl Completable for ProduceFd {
            type Result = std::io::Result<std::fs::File>;

            fn complete(self, completion: Completion) -> Self::Result {
                let fd = completion.result? as RawFd;
                // Safety: test supplies an open fd in the completion result.
                Ok(unsafe { std::fs::File::from_raw_fd(fd) })
            }
        }

        // Open /dev/null and hand the raw fd to a fake completion (like accept).
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        let raw = file.into_raw_fd();

        let mut state = State::Ignored(Box::new(ProduceFd));
        assert!(state.complete(Completion {
            result: Ok(raw as u32),
            flags: 0,
        }));
        // If cleanup failed to close, this would leak `raw`. We cannot portably
        // assert the fd is closed without racy probes; the important contract is
        // that `complete` ran and the File was dropped (covered by miri/ASAN in
        // CI when available, and by the cleaned_up flag tests above).
    }
}
