use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{
    io::Result,
    task::{Context, Poll},
    time::Duration,
};

use io_uring::opcode::AsyncCancel;
use io_uring::{Builder, IoUring, cqueue, squeue};

use slab::Slab;

use crate::driver::Handle;
use crate::driver::{
    backends::DriverBackend,
    ops::{Completion, Op, Operable, State, Submittable},
};

pub(crate) type Submission = squeue::Entry;

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
    pub(crate) fn new() -> Result<Self> {
        let eventfd = unsafe {
            let fd = libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        let wakeup_buf = Box::into_raw(Box::new([0u8; 8]));

        let mut backend = Self {
            // TODO: Make this configurable later
            ops: Slab::with_capacity(1024),
            uring: IoUring::builder().build(1024)?,
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

impl DriverBackend for IoUringBackend {
    fn submit_op<T: Submittable>(&mut self, mut data: T, handle: Handle) -> Result<Op<T>> {
        // Allocate a new entry in the driver
        let index = self.ops.insert(State::Submitted);

        // Submit the new operation to the kernel
        let entry = data.submit().user_data(index as _);

        while unsafe { self.uring.submission().push(&entry).is_err() } {
            // If the submission queue is full, flush it to the kernel
            self.submit()?;
        }

        // Create a new operation and assign the driver entry
        Ok(Op::<T>::new(index, data, handle))
    }

    fn remove_op<T: 'static>(&mut self, op: &mut Op<T>) {
        // Get the op state from the driver
        let state = match self.ops.get_mut(op.index()) {
            Some(val) => val,
            None => {
                // Op already dropped or removed
                return;
            }
        };

        match state {
            State::Submitted | State::Waiting(..) => {
                *state = State::Ignored(Box::new(op.take_data()));
            }
            State::Completed(..) => {
                self.ops.remove(op.index());
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
            State::Ready => unreachable!("invalid operation state"),
        }
    }

    fn poll_op<T: Operable>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        // IoUring is pure async, so we don't need this.
        // This is only here to match the overall driver interface signature
        _blocking: &crate::runtime::blocking::BlockingPoolHandle,
        _wakeup: &crate::driver::Wakeup,
    ) -> Poll<T::Result> {
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
                State::Completed(completion) => Poll::Ready(op.take_data().unwrap().complete(completion)),
                _ => unreachable!("invalid operation"),
            },
            // The op has been ignored/cancelled by the caller. It should not be polled again
            State::Ignored(..) => {
                unreachable!("invalid operation")
            }
            // This state is only set in poll based reactors, not completion reactors
            State::Ready => {
                unreachable!("invalid operation")
            }
        }
    }

    fn submit(&mut self) -> Result<()> {
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

    fn wait(&mut self) -> Result<usize> {
        self.uring.submit_and_wait(1)
    }

    fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
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

    fn dispatch_completions(&mut self) {
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
            let flags = completion.flags();
            let result = if res >= 0 {
                Ok(res as u32)
            } else {
                Err(std::io::Error::from_raw_os_error(-res))
            };

            if self.ops[index].complete(Completion { result, flags }) {
                self.ops.remove(index);
            }
        }

        if rearm_wakeup {
            self.arm_wakeup_read();
        }
    }

    fn create_wakeup(&self) -> crate::driver::Wakeup {
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

        // Pre-determine what to cancel
        // After this pass, all ops will be marked either as Completed or Ignored, as appropriate
        for (_, state) in self.ops.iter_mut() {
            match std::mem::replace(state, State::Ignored(Box::new(()))) {
                old_state @ State::Completed(_) => {
                    // Don't cancel completed ops
                    *state = old_state;
                }
                _ => {
                    // All other states need cancelling.
                    // The mem::replace means these are now marked Ignored.
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
