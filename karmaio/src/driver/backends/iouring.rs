use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::{
    io::Result,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use io_uring::opcode::AsyncCancel;
use io_uring::{IoUring, squeue};
use rustix::event::{EventfdFlags, eventfd};
use rustix::io::write;

use crate::driver::ops::{Completion, CompletionKey, DeferredAction, Op, OpKey, OpTable};
use crate::driver::{Handle, Wakeup};

pub(crate) type Submission = squeue::Entry;

/// Backend-local protocol for one-shot io_uring operations.
///
/// Implementations must keep every pointer embedded in the returned SQE valid
/// until the operation's terminal CQE. The operation payload remains owned by
/// the typed [`Op`] future, or by the exceptional detached cleanup record,
/// until [`UringOperation::complete`] has run.
///
/// # Lifecycle
///
/// [`UringOperation::submit`] is called from `IoUringBackend::submit_op` when
/// the future is constructed. The SQE is pushed (and flushed if the SQ is full)
/// before `submit_op` returns. [`UringOperation::complete`] runs after the
/// terminal CQE, outside the driver's backend borrow.
///
/// # Safety
///
/// Implementations must ensure that every pointer embedded in the returned
/// SQE remains valid and points to the correct operation-owned storage until
/// the terminal CQE has been dispatched.
pub(crate) unsafe trait UringOperation: 'static {
    type Output;

    /// Build the one-shot SQE for this operation (invoked during `submit_op`).
    fn submit(&mut self) -> Submission;

    /// Convert the terminal CQE into the operation's typed result.
    fn complete(self, completion: Completion) -> Self::Output;
}

enum State {
    Submitted,
    Waiting(Waker),
    Completed(Completion),
    Ignored(Box<dyn IgnoredOp>),
}

trait IgnoredOp: 'static {
    fn cleanup(self: Box<Self>, completion: Completion);
}

impl<T: UringOperation + 'static> IgnoredOp for T {
    fn cleanup(self: Box<Self>, completion: Completion) {
        drop(UringOperation::complete(*self, completion));
    }
}

struct Detached;

impl IgnoredOp for Detached {
    fn cleanup(self: Box<Self>, _completion: Completion) {}
}

impl State {
    /// Apply a terminal completion.
    ///
    /// Returns `(remove_slot, wake, deferred_cleanup)`. Detached cleanup and
    /// task wakes are returned rather than run immediately so the driver can
    /// release its backend borrow first.
    fn complete(&mut self, completion: Completion) -> (bool, Option<Waker>, Option<DeferredAction>) {
        match self {
            State::Submitted => {
                *self = State::Completed(completion);
                (false, None, None)
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                if let State::Waiting(waker) = old {
                    return (false, Some(waker), None);
                }
                (false, None, None)
            }
            State::Ignored(_) => {
                if let State::Ignored(payload) = std::mem::replace(self, State::Submitted) {
                    let action = DeferredAction::new(move || payload.cleanup(completion));
                    return (true, None, Some(action));
                }
                (true, None, None)
            }
            // A duplicate CQE is stale input. Keep the first terminal result
            // and let the future consume it normally.
            State::Completed(..) => (false, None, None),
        }
    }
}

struct EventFdWakeup {
    fd: OwnedFd,
    closed: AtomicBool,
}

impl EventFdWakeup {
    fn wake(&self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }

        let val: u64 = 1;
        let _ = write(&self.fd, &val.to_ne_bytes());
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

pub(crate) struct IoUringBackend {
    // List of ops tracked by the driver
    ops: OpTable<State>,

    // IoUring bindings
    uring: IoUring,

    /// eventfd used for cross-thread wakeups. We keep a read armed on it
    /// so that writes from other threads produce a CQE and wake submit_and_wait.
    eventfd: Arc<EventFdWakeup>,

    /// Persistent buffer for the armed wakeup read. The kernel writes the eventfd
    /// counter here while the read is in flight. We reuse the same allocation.
    wakeup_buf: Pin<Box<[u8; 8]>>,
    wakeup_read_armed: bool,
    shutting_down: bool,
}

impl IoUringBackend {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let eventfd = Arc::new(EventFdWakeup {
            fd: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            closed: AtomicBool::new(false),
        });

        let wakeup_buf = Box::pin([0u8; 8]);

        let mut backend = Self {
            // `capacity` is configurable via the runtime builder's driver capacity.
            ops: OpTable::new(capacity)?,
            uring: IoUring::builder().build(capacity as u32)?,
            eventfd,
            wakeup_buf,
            wakeup_read_armed: false,
            shutting_down: false,
        };

        // Arm the initial read on the eventfd so that a write from another
        // thread will complete and wake any blocked submit_and_wait.
        backend.arm_wakeup_read()?;

        Ok(backend)
    }

    /// Submit (or re-arm) an async read on the eventfd using a special user_data.
    /// We bypass the normal Op machinery for the wakeup eventfd.
    fn arm_wakeup_read(&mut self) -> Result<()> {
        use io_uring::opcode;

        let buf_ptr = self.wakeup_buf.as_mut().get_mut().as_mut_ptr();

        let read_e = opcode::Read::new(io_uring::types::Fd(self.eventfd.fd.as_raw_fd()), buf_ptr, 8)
            .build()
            .user_data(CompletionKey::wake_raw() as u64);

        // Best effort push; if full we submit first.
        while unsafe { self.uring.submission().push(&read_e).is_err() } {
            self.submit()?;
        }

        self.wakeup_read_armed = true;
        Ok(())
    }
}

impl IoUringBackend {
    pub(crate) fn submit_op<T: UringOperation + 'static>(&mut self, mut data: T, handle: Handle) -> Result<Op<T>> {
        if self.shutting_down {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io_uring backend is shutting down",
            ));
        }

        // Allocate a new entry in the driver
        let key = self.ops.insert(State::Submitted)?;

        // Submit the new operation to the kernel
        let entry = UringOperation::submit(&mut data).user_data(key.as_u64());

        while unsafe { self.uring.submission().push(&entry).is_err() } {
            // If the submission queue is full, flush it to the kernel
            if let Err(error) = self.submit() {
                let _ = self.ops.remove(key);
                return Err(error);
            }
        }

        // Create a new operation and assign the driver entry
        Ok(Op::<T>::new(key, data, handle))
    }

    /// Detach or cancel an operation.
    ///
    /// When the operation already has a terminal completion that was never
    /// polled, returns that completion so the driver can run typed `complete`
    /// after releasing the backend borrow. The payload remains in `op`.
    pub(crate) fn remove_op<T: UringOperation + 'static>(&mut self, op: &mut Op<T>) -> Option<Completion> {
        let key = op.key();
        // Get the op state from the driver
        let state = match self.ops.get_mut(key) {
            Some(val) => val,
            None => {
                // Op already dropped or removed
                return None;
            }
        };

        match state {
            // Detach only: keep payload alive until the CQE so buffers stay valid
            // and so a late completion can cleanup produced FDs (accept/open).
            // We do not submit AsyncCancel here; cancellation is reserved for driver Drop.
            State::Submitted | State::Waiting(..) => {
                let data = op.take_data().expect("op data missing on detach");
                *state = State::Ignored(Box::new(data));
                None
            }
            State::Completed(_) => {
                // Completion already arrived but the future never polled it.
                match self.ops.remove(key) {
                    Some(State::Completed(completion)) => Some(completion),
                    _ => None,
                }
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    /// Drive one poll of an io_uring operation.
    ///
    /// On `Poll::Ready`, the terminal [`Completion`] is returned and the op
    /// payload is left in `op` so the driver can run typed `complete` after
    /// releasing the backend borrow.
    pub(crate) fn poll_op<T: UringOperation + 'static>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
    ) -> Poll<Completion> {
        // Get the op state from the driver
        let state = self.ops.get_mut(op.key()).expect("invalid internal state");

        match state {
            // Op has been submitted to the kernel. Assign the waker for completion
            State::Submitted => {
                *state = State::Waiting(cx.waker().clone());
                Poll::Pending
            }
            // Kernel has not yet completed the op. Continue waiting
            State::Waiting(waker) => {
                if !waker.will_wake(cx.waker()) {
                    // A different waker has been received. Update the state with the new waker
                    *state = State::Waiting(cx.waker().clone());
                }
                Poll::Pending
            }
            // The kernel has completed the op. Resolve the future with the result
            State::Completed(_) => match self.ops.remove(op.key()) {
                Some(State::Completed(completion)) => Poll::Ready(completion),
                _ => unreachable!("invalid operation"),
            },
            // The op has been ignored/cancelled by the caller. It should not be polled again
            State::Ignored(..) => {
                unreachable!("invalid operation")
            }
        }
    }

    pub(crate) fn submit(&mut self) -> Result<()> {
        loop {
            match self.uring.submit() {
                Ok(_) => {
                    self.uring.submission().sync();
                    return Ok(());
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    // Run deferred work immediately: submit can re-enter while
                    // the backend is already exclusively owned (no outer Driver
                    // RefCell borrow on this path).
                    DeferredAction::run_all(self.dispatch_completions()?);
                }
                Err(e) if e.raw_os_error() != Some(libc::EINTR) => {
                    return Err(e);
                }
                _ => continue,
            }
        }
    }

    pub(crate) fn wait(&mut self) -> Result<usize> {
        self.uring.submit_and_wait(1)
    }

    pub(crate) fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        let timeout = io_uring::types::Timespec::from(duration);
        let args = io_uring::types::SubmitArgs::new().timespec(&timeout);

        loop {
            match self.uring.submitter().submit_with_args(1, &args) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.raw_os_error() == Some(libc::ETIME) => {
                    return Ok(0);
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn dispatch_completions(&mut self) -> Result<Vec<DeferredAction>> {
        let mut completion_queue = self.uring.completion();

        completion_queue.sync();

        // Re-arm the eventfd read after the completion queue borrow is released,
        // since `arm_wakeup_read` needs a mutable borrow of `self`.
        let mut rearm_wakeup = false;
        let mut deferred = Vec::new();

        for completion in completion_queue {
            let Some(key) = CompletionKey::decode(completion.user_data()) else {
                // A malformed or stale control value cannot name an operation.
                continue;
            };

            let key = match key {
                CompletionKey::Cancel | CompletionKey::WakeCancel => {
                    // Result of a cancellation SQE. The target operation's own
                    // CQE is still required before its slot can be retired.
                    continue;
                }
                CompletionKey::Wake => {
                    // Wakeup from the eventfd. Re-arm another read so future wakes work.
                    // The written counter bytes in wakeup_buf can be ignored.
                    self.wakeup_read_armed = false;
                    rearm_wakeup = !self.shutting_down;
                    continue;
                }
                CompletionKey::Operation(key) => key,
            };

            let res = completion.result();
            let result = if res >= 0 {
                Ok(res as u32)
            } else {
                Err(std::io::Error::from_raw_os_error(-res))
            };

            if let Some(state) = self.ops.get_mut(key) {
                let (remove, waker, cleanup) =
                    state.complete(Completion::with_flags(result, completion.flags()));
                if let Some(waker) = waker {
                    deferred.push(DeferredAction::new(move || waker.wake()));
                }
                if let Some(cleanup) = cleanup {
                    deferred.push(cleanup);
                }
                if remove {
                    self.ops.remove(key);
                }
            }
        }

        // Drop the completion queue borrow before re-arming.
        drop(completion_queue);

        if rearm_wakeup {
            self.arm_wakeup_read()?;
        }

        Ok(deferred)
    }

    pub(crate) fn create_wakeup(&self) -> Wakeup {
        let wakeup = Arc::clone(&self.eventfd);
        crate::driver::Wakeup::new(move || wakeup.wake())
    }

    pub(crate) fn drain_blocking_completions(&mut self) -> Vec<DeferredAction> {
        Vec::new()
    }
}

impl AsRawFd for IoUringBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.uring.as_raw_fd()
    }
}

// Drop the driver, cancelling any in-progress ops and waiting for them to terminate.
//
// This first cancels all ops and then waits for them to be moved to the completed state phase.
//
// It is possible for this to be run without previously dropping the runtime,
// but this should only be possible in the case of [`std::process::exit`].
//
// This depends on us knowing when ops are completed and done firing.
impl IoUringBackend {
    /// Stop accepting work, cancel in-flight requests, and drain their CQEs
    /// before releasing kernel-visible buffers and operation payloads.
    ///
    /// Runtime teardown calls this explicitly after the blocking pool has
    /// joined. `Drop` remains as a final backstop for standalone backend use.
    pub(crate) fn shutdown(&mut self) -> Vec<DeferredAction> {
        if self.shutting_down {
            return Vec::new();
        }
        self.shutting_down = true;
        self.eventfd.close();

        // get all ops in flight for cancellation
        while !self.uring.submission().is_empty() {
            self.submit().expect("Internal error when dropping driver");
        }

        // Pre-determine what to cancel.
        // After this pass, all ops are Completed or Ignored.
        // Preserve existing Ignored payloads so late CQEs still cleanup orphan FDs.
        for (_, state) in self.ops.iter_mut() {
            match std::mem::replace(state, State::Ignored(Box::new(Detached))) {
                old_state @ State::Completed(_) => {
                    // Don't cancel completed ops
                    *state = old_state;
                }
                State::Ignored(payload) => {
                    // Keep the typed cleanup installed by Op::drop.
                    *state = State::Ignored(payload);
                }
                _ => {
                    // Submitted / Waiting without a detached payload (driver
                    // dropped before Op::drop). No typed cleanup available.
                }
            }
        }

        // Submit cancellation for all ops marked Ignored
        let cancelled: Vec<OpKey> = self
            .ops
            .iter()
            .filter_map(|(key, state)| matches!(state, State::Ignored(..)).then_some(key))
            .collect();
        for key in cancelled {
            unsafe {
                while self
                    .uring
                    .submission()
                    .push(
                        &AsyncCancel::new(key.as_u64())
                            .build()
                            .user_data(CompletionKey::cancel_raw() as u64),
                    )
                    .is_err()
                {
                    self.uring
                        .submit_and_wait(1)
                        .expect("Internal error when dropping driver");
                }
            }
        }

        // The wakeup read owns the pinned buffer until its CQE arrives. Cancel
        // it before the io_uring and buffer fields are dropped.
        if self.wakeup_read_armed {
            unsafe {
                while self
                    .uring
                    .submission()
                    .push(
                        &AsyncCancel::new(CompletionKey::wake_raw() as u64)
                            .build()
                            .user_data(CompletionKey::wake_cancel_raw() as u64),
                    )
                    .is_err()
                {
                    self.uring
                        .submit_and_wait(1)
                        .expect("Internal error when cancelling wakeup read");
                }
            }
            self.submit().expect("Internal error when cancelling wakeup read");
            while self.wakeup_read_armed {
                self.wait().expect("Internal error when draining wakeup read");
                DeferredAction::run_all(
                    self.dispatch_completions()
                        .expect("Internal error when draining wakeup read"),
                );
            }
        }

        // Wait until all ops have been removed from the slab.
        // Ignored entries will be removed from the slab by the complete logic called by `tick()`
        // Completed Entries are removed here directly
        loop {
            if self.ops.is_empty() {
                // All ops are drained. We can shutdown
                break;
            }

            // States are either all ignored or complete
            // If there is at least one Ignored still to process, call wait
            let Some((key, state)) = self.ops.iter().next() else {
                break;
            };
            match state {
                State::Ignored(..) => {
                    // If waiting fails, ignore the error.
                    // The wait will be attempted again on the next loop.
                    let _ = self.wait();
                    DeferredAction::run_all(
                        self.dispatch_completions()
                            .expect("Internal error when dropping driver"),
                    );
                }
                _ => {
                    // Remove completed ops
                    let _ = self.ops.remove(key);
                }
            }
        }

        // Final sanity check, any ops must be in complete state
        assert!(self.ops.iter().all(|(_, state)| matches!(state, State::Completed(..))));
        Vec::new()
    }
}

impl Drop for IoUringBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::Wake;

    struct CleanupMarker(Arc<AtomicBool>);

    unsafe impl UringOperation for CleanupMarker {
        type Output = ();

        fn submit(&mut self) -> Submission {
            io_uring::opcode::Nop::new().build()
        }

        fn complete(self, _completion: Completion) -> Self::Output {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct WakeMarker(Arc<AtomicBool>);

    impl Wake for WakeMarker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn completion_before_first_poll_is_retained() {
        let mut state = State::Submitted;
        let (remove, wake, cleanup) = state.complete(Completion::new(Ok(7)));
        assert!(!remove);
        assert!(wake.is_none());
        assert!(cleanup.is_none());
        assert!(matches!(state, State::Completed(..)));
    }

    #[test]
    fn detached_completion_runs_typed_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut state = State::Ignored(Box::new(CleanupMarker(cleaned.clone())));

        let (remove, wake, cleanup) = state.complete(Completion::new(Ok(0)));
        assert!(remove);
        assert!(wake.is_none());
        cleanup.expect("detached cleanup should be deferred").run();
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[test]
    fn completion_wakes_the_current_waiter() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = std::task::Waker::from(Arc::new(WakeMarker(woken.clone())));
        let mut state = State::Waiting(waker);

        let (remove, wake, cleanup) = state.complete(Completion::new(Ok(0)));
        assert!(!remove);
        assert!(cleanup.is_none());
        wake.expect("waiting state should return its waker").wake();
        assert!(woken.load(Ordering::SeqCst));
    }
}
