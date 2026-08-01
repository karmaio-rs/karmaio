use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{
    io::Result,
    task::{Context, Poll, Waker},
    time::Duration,
};

use io_uring::opcode::AsyncCancel;
use io_uring::{IoUring, squeue};

use slab::Slab;

use crate::driver::ops::{Completion, Op};
use crate::driver::{Handle, Wakeup};

pub(crate) type Submission = squeue::Entry;

/// Build and consume a normal one-shot io_uring operation.
pub(crate) trait UringSubmit {
    fn submit(&mut self) -> Submission;
}

pub(crate) trait UringComplete {
    type Result;

    fn complete(self, completion: Completion) -> Self::Result;
}

/// Backend-local operation protocol used by the typed io_uring future.
pub(crate) trait UringOperation: UringSubmit + UringComplete {
    type Output;

    fn submit(&mut self) -> Submission;
    fn complete(self, completion: Completion) -> Self::Output;
}

impl<T: UringSubmit + UringComplete> UringOperation for T {
    type Output = T::Result;

    #[inline]
    fn submit(&mut self) -> Submission {
        UringSubmit::submit(self)
    }

    #[inline]
    fn complete(self, completion: Completion) -> Self::Output {
        UringComplete::complete(self, completion)
    }
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
    fn complete(&mut self, completion: Completion) -> bool {
        match self {
            State::Submitted => {
                *self = State::Completed(completion);
                false
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                if let State::Waiting(waker) = old {
                    waker.wake();
                }
                false
            }
            State::Ignored(_) => {
                if let State::Ignored(payload) = std::mem::replace(self, State::Submitted) {
                    payload.cleanup(completion);
                }
                true
            }
            State::Completed(..) => unreachable!("completion delivered twice"),
        }
    }
}

pub(crate) struct IoUringBackend {
    // List of ops tracked by the driver
    ops: Slab<State>,

    // IoUring bindings
    uring: IoUring,

    /// eventfd used for cross-thread wakeups. We keep a read armed on it
    /// so that writes from other threads produce a CQE and wake submit_and_wait.
    eventfd: std::os::fd::OwnedFd,

    /// Persistent buffer for the armed wakeup read. The kernel writes the eventfd
    /// counter here while the read is in flight. We reuse the same allocation.
    wakeup_buf: *mut [u8; 8],
}

impl IoUringBackend {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let eventfd = unsafe {
            let fd = libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        let wakeup_buf = Box::into_raw(Box::new([0u8; 8]));

        let mut backend = Self {
            // `capacity` is configurable via the runtime builder's driver capacity.
            ops: Slab::with_capacity(capacity),
            uring: IoUring::builder().build(capacity as u32)?,
            eventfd,
            wakeup_buf,
        };

        // Arm the initial read on the eventfd so that a write from another
        // thread will complete and wake any blocked submit_and_wait.
        backend.arm_wakeup_read();

        Ok(backend)
    }

    /// Submit (or re-arm) an async read on the eventfd using a special user_data.
    /// We bypass the normal Op machinery for the wakeup eventfd.
    fn arm_wakeup_read(&mut self) {
        use io_uring::opcode;

        // Use a special user_data that we recognize in dispatch.
        // u64::MAX-1 to avoid conflict with cancel (u64::MAX).
        const WAKE_USERDATA: u64 = u64::MAX - 1;

        let buf_ptr = self.wakeup_buf as *mut u8;

        let read_e = opcode::Read::new(io_uring::types::Fd(self.eventfd.as_raw_fd()), buf_ptr, 8)
            .build()
            .user_data(WAKE_USERDATA);

        // Best effort push; if full we submit first.
        while unsafe { self.uring.submission().push(&read_e).is_err() } {
            let _ = self.submit();
        }
    }
}

impl IoUringBackend {
    pub(crate) fn submit_op<T: UringOperation + 'static>(&mut self, mut data: T, handle: Handle) -> Result<Op<T>> {
        // Allocate a new entry in the driver
        let index = self.ops.insert(State::Submitted);

        // Submit the new operation to the kernel
        let entry = UringOperation::submit(&mut data).user_data(index as _);

        while unsafe { self.uring.submission().push(&entry).is_err() } {
            // If the submission queue is full, flush it to the kernel
            self.submit()?;
        }

        // Create a new operation and assign the driver entry
        Ok(Op::<T>::new(index, data, handle))
    }

    pub(crate) fn remove_op<T: UringOperation + 'static>(&mut self, op: &mut Op<T>) {
        let index = op.index();
        // Get the op state from the driver
        let state = match self.ops.get_mut(index) {
            Some(val) => val,
            None => {
                // Op already dropped or removed
                return;
            }
        };

        match state {
            // Detach only: keep payload alive until the CQE so buffers stay valid
            // and so a late completion can cleanup produced FDs (accept/open).
            // We do not submit AsyncCancel here; cancellation is reserved for driver Drop.
            State::Submitted | State::Waiting(..) => {
                let data = op.take_data().expect("op data missing on detach");
                *state = State::Ignored(Box::new(data));
            }
            State::Completed(_) => {
                // Completion already arrived but the future never polled it.
                // Run complete so orphan accept/open FDs are closed.
                if let State::Completed(completion) = self.ops.remove(index) {
                    if let Some(data) = op.take_data() {
                        drop(UringOperation::complete(data, completion));
                    }
                }
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    pub(crate) fn poll_op<T: UringOperation + 'static>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
    ) -> Poll<T::Output> {
        // Get the op state from the driver
        let state = self.ops.get_mut(op.index()).expect("invalid internal state");

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
            State::Completed(_) => match self.ops.remove(op.index()) {
                State::Completed(completion) => {
                    Poll::Ready(UringOperation::complete(op.take_data().unwrap(), completion))
                }
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
                    self.dispatch_completions();
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

    pub(crate) fn dispatch_completions(&mut self) {
        let mut completion_queue = self.uring.completion();

        completion_queue.sync();

        // Re-arm the eventfd read after the completion queue borrow is released,
        // since `arm_wakeup_read` needs a mutable borrow of `self`.
        // TODD: Come back to see if there is a better approach for this
        let mut rearm_wakeup = false;

        for completion in completion_queue {
            if completion.user_data() == u64::MAX {
                // Result of the cancellation action.
                // There isn't anything we need to do here.
                // We must wait for the CQE for the operation that was canceled.
                continue;
            }

            const WAKE_USERDATA: u64 = u64::MAX - 1;
            if completion.user_data() == WAKE_USERDATA {
                // Wakeup from the eventfd. Re-arm another read so future wakes work.
                // The written counter bytes in wakeup_buf can be ignored.
                rearm_wakeup = true;
                continue;
            }

            let index = completion.user_data() as usize;
            let res = completion.result();
            let result = if res >= 0 {
                Ok(res as u32)
            } else {
                Err(std::io::Error::from_raw_os_error(-res))
            };

            if self.ops[index].complete(Completion { result }) {
                self.ops.remove(index);
            }
        }

        if rearm_wakeup {
            self.arm_wakeup_read();
        }
    }

    pub(crate) fn create_wakeup(&self) -> Wakeup {
        let fd = self.eventfd.as_raw_fd();
        crate::driver::Wakeup::new(move || {
            let val: u64 = 1;
            let _ = unsafe {
                libc::write(
                    fd,
                    &val as *const u64 as *const libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
        })
    }

    pub(crate) fn attach(&self, _fd: RawFd) -> Result<()> {
        // No-op on io_uring: handles don't need explicit registration.
        Ok(())
    }

    pub(crate) fn drain_blocking_completions(&mut self) {}
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
impl Drop for IoUringBackend {
    fn drop(&mut self) {
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
        for (index, state) in self.ops.iter_mut() {
            if let State::Ignored(..) = state {
                unsafe {
                    while self
                        .uring
                        .submission()
                        .push(&AsyncCancel::new(index as u64).build().user_data(u64::MAX))
                        .is_err()
                    {
                        self.uring
                            .submit_and_wait(1)
                            .expect("Internal error when dropping driver");
                    }
                }
            }
        }

        // Wait until all ops have been removed from the slab.
        // Ignored entries will be removed from the slab by the complete logic called by `tick()`
        // Completed Entries are removed here directly
        let mut index = 0;
        loop {
            if self.ops.is_empty() {
                // All ops are drained. We can shutdown
                break;
            }

            // States are either all ignored or complete
            // If there is at least one Ignored still to process, call wait
            match self.ops.get(index) {
                Some(State::Ignored(..)) => {
                    // If waiting fails, ignore the error.
                    // The wait will be attempted again on the next loop.
                    let _ = self.wait();
                    self.dispatch_completions();
                }
                Some(_) => {
                    // Remove completed ops
                    let _ = self.ops.remove(index);
                    index += 1;
                }
                None => {
                    index += 1;
                }
            }
        }

        // Final sanity check, any ops must be in complete state
        assert!(self.ops.iter().all(|(_, state)| matches!(state, State::Completed(..))));

        // Free the wakeup buffer (one persistent allocation for the lifetime of the backend).
        if !self.wakeup_buf.is_null() {
            unsafe {
                drop(Box::from_raw(self.wakeup_buf));
            }
        }
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

    impl UringSubmit for CleanupMarker {
        fn submit(&mut self) -> Submission {
            io_uring::opcode::Nop::new().build()
        }
    }

    impl UringComplete for CleanupMarker {
        type Result = ();

        fn complete(self, _completion: Completion) -> Self::Result {
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
        assert!(!state.complete(Completion { result: Ok(7) }));
        assert!(matches!(state, State::Completed(..)));
    }

    #[test]
    fn detached_completion_runs_typed_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut state = State::Ignored(Box::new(CleanupMarker(cleaned.clone())));

        assert!(state.complete(Completion { result: Ok(0) }));
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[test]
    fn completion_wakes_the_current_waiter() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = std::task::Waker::from(Arc::new(WakeMarker(woken.clone())));
        let mut state = State::Waiting(waker);

        assert!(!state.complete(Completion { result: Ok(0) }));
        assert!(woken.load(Ordering::SeqCst));
    }
}
