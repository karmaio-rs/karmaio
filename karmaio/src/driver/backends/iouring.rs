use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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

use io_uring::cqueue;
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

/// Backend-local protocol for multishot io_uring operations.
///
/// A single SQE may produce many CQEs. Intermediate CQEs have
/// `IORING_CQE_F_MORE` set; the final CQE does not. Completions are queued on
/// the driver lifecycle and converted to [`Item`](UringMultishotOperation::Item)
/// when the multishot stream is polled.
///
/// Multishot APIs require Linux 6.12+. karmaio does not probe the kernel
/// version; callers must ensure they meet that floor.
///
/// # Safety
///
/// Same pointer-validity rules as [`UringOperation`]: every pointer embedded in
/// the SQE must remain valid until the multishot request has fully terminated
/// (final CQE without `MORE`).
pub(crate) unsafe trait UringMultishotOperation: 'static {
    /// One stream item produced from a single CQE.
    type Item;

    /// Build the multishot SQE (invoked during `submit_multi_op`).
    fn submit(&mut self) -> Submission;

    /// Convert one CQE into a stream item.
    fn complete_item(&mut self, completion: Completion) -> Self::Item;

    /// Drop a completion that will never be delivered (cancel, stream drop, or
    /// driver shutdown). Must reclaim any kernel-transferred resources (for
    /// example accepted file descriptors).
    fn discard_item(&mut self, completion: Completion);
}

/// Type-erased cleanup for multishot completions after the stream is dropped.
trait MultishotCleanup: 'static {
    fn discard(&mut self, completion: Completion);
}

impl<T: UringMultishotOperation> MultishotCleanup for T {
    fn discard(&mut self, completion: Completion) {
        self.discard_item(completion);
    }
}

/// Close a completion result that is known to own a raw file descriptor.
///
/// Used only when typed multishot cleanup is unavailable (orphan shutdown).
fn discard_completion_as_fd(completion: Completion) {
    if let Ok(fd) = completion.result {
        // Safety: accept-style multishot CQEs transfer ownership of a new fd in
        // `res`. On orphan shutdown we have no typed op to reclaim it otherwise.
        drop(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
    }
}

enum State {
    Submitted,
    Waiting(Waker),
    Completed(Completion),
    Ignored(Box<dyn IgnoredOp>),

    /// Multishot request still armed in the kernel.
    MultishotActive {
        waker: Option<Waker>,
        pending: VecDeque<Completion>,
    },
    /// Final CQE received (`!MORE`); drain remaining items then free the slot.
    MultishotFinished {
        pending: VecDeque<Completion>,
    },
    /// Stream dropped; cancel in flight. Discard late CQEs with typed cleanup.
    MultishotCancelled {
        pending: VecDeque<Completion>,
        cleanup: Box<dyn MultishotCleanup>,
    },
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
    /// Apply a terminal oneshot completion.
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
            State::MultishotActive { .. } | State::MultishotFinished { .. } | State::MultishotCancelled { .. } => {
                unreachable!("oneshot complete on multishot state")
            }
        }
    }

    /// Apply a multishot CQE.
    ///
    /// `more` is `IORING_CQE_F_MORE` from the CQE flags.
    fn push_multishot(&mut self, completion: Completion, more: bool) -> (bool, Option<Waker>, Option<DeferredAction>) {
        match self {
            State::MultishotActive { waker, pending } => {
                pending.push_back(completion);
                let wake = waker.take();
                if !more {
                    let pending = std::mem::take(pending);
                    *self = State::MultishotFinished { pending };
                }
                (false, wake, None)
            }
            State::MultishotCancelled { pending, cleanup } => {
                if more {
                    cleanup.discard(completion);
                    (false, None, None)
                } else {
                    // Final CQE after cancel: discard everything and free the slot.
                    cleanup.discard(completion);
                    for item in pending.drain(..) {
                        cleanup.discard(item);
                    }
                    (true, None, None)
                }
            }
            // Finished should not receive further CQEs; ignore stale input.
            State::MultishotFinished { .. } => (false, None, None),
            State::Submitted | State::Waiting(_) | State::Completed(_) | State::Ignored(_) => {
                unreachable!("multishot CQE on oneshot state")
            }
        }
    }

    fn is_multishot(&self) -> bool {
        matches!(
            self,
            State::MultishotActive { .. } | State::MultishotFinished { .. } | State::MultishotCancelled { .. }
        )
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

    fn push_cancel(&mut self, key: OpKey) {
        let entry = AsyncCancel::new(key.as_u64())
            .build()
            .user_data(CompletionKey::cancel_raw() as u64);
        while unsafe { self.uring.submission().push(&entry).is_err() } {
            let _ = self.submit();
        }
    }
}

impl IoUringBackend {
    pub(crate) fn submit_op<T: UringOperation + 'static>(&mut self, data: T, handle: Handle) -> Result<Op<T>> {
        if self.shutting_down {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io_uring backend is shutting down",
            ));
        }

        // Stabilize the operation before deriving any pointer that may be
        // embedded in the SQE. Moving the returned future only moves this box.
        let mut data = Box::new(data);

        // Allocate a new entry in the driver
        let key = self.ops.insert(State::Submitted)?;

        // Submit the new operation to the kernel
        let entry = UringOperation::submit(&mut *data).user_data(key.as_u64());

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

    /// Submit a multishot operation. Returns the op key and boxed payload.
    ///
    /// The caller wraps these in [`crate::driver::ops::MultiOp`].
    pub(crate) fn submit_multi_op<T: UringMultishotOperation + 'static>(
        &mut self,
        mut data: T,
    ) -> Result<(OpKey, Box<T>)> {
        if self.shutting_down {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io_uring backend is shutting down",
            ));
        }

        let key = self.ops.insert(State::MultishotActive {
            waker: None,
            pending: VecDeque::new(),
        })?;

        let entry = UringMultishotOperation::submit(&mut data).user_data(key.as_u64());

        while unsafe { self.uring.submission().push(&entry).is_err() } {
            if let Err(error) = self.submit() {
                let _ = self.ops.remove(key);
                return Err(error);
            }
        }

        Ok((key, Box::new(data)))
    }

    /// Detach or cancel an oneshot operation.
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
                *state = State::Ignored(data);
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
            State::MultishotActive { .. } | State::MultishotFinished { .. } | State::MultishotCancelled { .. } => {
                unreachable!("oneshot remove on multishot operation")
            }
        }
    }

    /// Cancel a multishot operation when its stream is dropped.
    ///
    /// Submits `AsyncCancel` while the request is still active. Pending and
    /// late CQEs are discarded through the operation's [`discard_item`].
    pub(crate) fn remove_multi_op<T: UringMultishotOperation + 'static>(&mut self, key: OpKey, mut data: Box<T>) {
        let Some(state) = self.ops.get_mut(key) else {
            return;
        };

        match state {
            State::MultishotActive { pending, .. } => {
                for completion in std::mem::take(pending) {
                    data.discard_item(completion);
                }
                *state = State::MultishotCancelled {
                    pending: VecDeque::new(),
                    cleanup: data,
                };
                self.push_cancel(key);
            }
            State::MultishotFinished { pending } => {
                for completion in std::mem::take(pending) {
                    data.discard_item(completion);
                }
                let _ = self.ops.remove(key);
            }
            State::MultishotCancelled { pending, cleanup } => {
                for completion in std::mem::take(pending) {
                    cleanup.discard(completion);
                }
            }
            State::Submitted | State::Waiting(_) | State::Completed(_) | State::Ignored(_) => {
                unreachable!("multishot remove on oneshot operation")
            }
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
            State::MultishotActive { .. } | State::MultishotFinished { .. } | State::MultishotCancelled { .. } => {
                unreachable!("oneshot poll on multishot operation")
            }
        }
    }

    /// Drive one poll of a multishot operation.
    ///
    /// Returns `Ready(Some(completion))` for a queued CQE, `Ready(None)` when
    /// the multishot request has fully ended and the queue is empty, or
    /// `Pending` while waiting for more CQEs.
    pub(crate) fn poll_multi_op(&mut self, key: OpKey, cx: &mut Context<'_>) -> Poll<Option<Completion>> {
        let state = self.ops.get_mut(key).expect("invalid multishot op state");

        match state {
            State::MultishotActive { waker, pending } => {
                if let Some(completion) = pending.pop_front() {
                    return Poll::Ready(Some(completion));
                }
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            State::MultishotFinished { pending } => {
                if let Some(completion) = pending.pop_front() {
                    return Poll::Ready(Some(completion));
                }
                let _ = self.ops.remove(key);
                Poll::Ready(None)
            }
            State::MultishotCancelled { .. } => {
                // Stream was dropped; further polls see end-of-stream.
                Poll::Ready(None)
            }
            State::Submitted | State::Waiting(_) | State::Completed(_) | State::Ignored(_) => {
                unreachable!("multishot poll on oneshot operation")
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

        for completion in &mut completion_queue {
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
            let flags = completion.flags();
            let more = cqueue::more(flags);
            let cqe = Completion::with_flags(result, flags);

            if let Some(state) = self.ops.get_mut(key) {
                let (remove, waker, cleanup) = if state.is_multishot() {
                    state.push_multishot(cqe, more)
                } else {
                    // Oneshot ops produce a single terminal CQE.
                    state.complete(cqe)
                };
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
        // After this pass, ops are Completed, Finished, Ignored, or MultishotCancelled.
        // Preserve existing Ignored / MultishotCancelled payloads for late CQEs.
        for (_, state) in self.ops.iter_mut() {
            match std::mem::replace(state, State::Ignored(Box::new(Detached))) {
                old_state @ State::Completed(_) => {
                    *state = old_state;
                }
                State::MultishotFinished { pending } => {
                    // Kernel already finished; reclaim any undelivered accept fds.
                    for completion in pending {
                        discard_completion_as_fd(completion);
                    }
                    // Leave a placeholder; the drain loop removes non-Ignored slots.
                    *state = State::Completed(Completion::new(Ok(0)));
                }
                State::Ignored(payload) => {
                    *state = State::Ignored(payload);
                }
                State::MultishotCancelled {
                    mut pending,
                    mut cleanup,
                } => {
                    for completion in pending.drain(..) {
                        cleanup.discard(completion);
                    }
                    *state = State::MultishotCancelled {
                        pending: VecDeque::new(),
                        cleanup,
                    };
                }
                State::MultishotActive { pending, .. } => {
                    for completion in pending {
                        discard_completion_as_fd(completion);
                    }
                    // Cancel without typed cleanup (orphan path).
                    *state = State::Ignored(Box::new(Detached));
                }
                State::Submitted | State::Waiting(_) => {
                    // Oneshot in flight without detached payload.
                }
            }
        }

        // Re-process MultishotCancelled entries that still have pending (none after above),
        // and ensure cancelled multishot ops get AsyncCancel.
        let to_cancel: Vec<OpKey> = self
            .ops
            .iter()
            .filter_map(|(key, state)| {
                matches!(state, State::Ignored(..) | State::MultishotCancelled { .. }).then_some(key)
            })
            .collect();
        for key in to_cancel {
            self.push_cancel(key);
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
        loop {
            if self.ops.is_empty() {
                break;
            }

            let Some((key, state)) = self.ops.iter().next() else {
                break;
            };
            match state {
                State::Ignored(..) | State::MultishotCancelled { .. } => {
                    let _ = self.wait();
                    DeferredAction::run_all(
                        self.dispatch_completions()
                            .expect("Internal error when dropping driver"),
                    );
                }
                _ => {
                    let _ = self.ops.remove(key);
                }
            }
        }

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

    #[test]
    fn multishot_more_keeps_active_and_queues() {
        let mut state = State::MultishotActive {
            waker: None,
            pending: VecDeque::new(),
        };
        let (remove, wake, cleanup) = state.push_multishot(Completion::new(Ok(3)), true);
        assert!(!remove);
        assert!(wake.is_none());
        assert!(cleanup.is_none());
        match &state {
            State::MultishotActive { pending, .. } => {
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].result.as_ref().unwrap(), &3);
            }
            _ => panic!("expected MultishotActive"),
        }
    }

    #[test]
    fn multishot_final_transitions_to_finished() {
        let mut state = State::MultishotActive {
            waker: None,
            pending: VecDeque::new(),
        };
        let _ = state.push_multishot(Completion::new(Ok(1)), true);
        let (remove, _, _) = state.push_multishot(Completion::new(Ok(2)), false);
        assert!(!remove);
        match &state {
            State::MultishotFinished { pending } => {
                assert_eq!(pending.len(), 2);
            }
            _ => panic!("expected MultishotFinished"),
        }
    }
}
