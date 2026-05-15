use std::os::fd::AsRawFd;
use std::{io::Result, task::Context, time::Duration};

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
}

impl IoUringBackend {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            // TODO: Make this configurable later
            ops: Slab::with_capacity(1024),
            uring: IoUring::builder().build(1024)?,
        })
    }
}

impl DriverBackend for IoUringBackend {
    fn submit_op<T: Submittable>(&mut self, data: T, handle: Handle) -> Result<Op<T>> {
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

    fn remove_op<T>(&mut self, op: &mut Op<T>) {
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

    fn poll_op<T: Operable>(&mut self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Result> {
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
            State::Completed(_) => {
                match self.ops.remove(op.index()) {
                    State::Completed(completion) => Poll::Ready(op.take_data().unwrap().complete(completion)),
                    _ => unreachable!("invalid operation"),
                };
            }
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

        for completion in completion_queue {
            if completion.user_data() == u64::MAX {
                // Result of the cancellation action.
                // There isn't anything we need to do here.
                // We must wait for the CQE for the operation that was canceled.
                continue;
            }

            let index = completion.user_data() as usize;
            let res = completion.result();
            let flags = completion.flags();
            let result = if res >= 0 {
                Ok(res as u32)
            } else {
                Err(io::Error::from_raw_os_error(-res))
            };

            if self.ops[index].complete(Completion { result, flags }) {
                self.ops.remove(index);
            }
        }
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
        assert!(self.ops.iter().all(|(_, state)| matches!(state, State::Completed(..))))
    }
}
