use std::{
    io::{Error, Result},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    task::{Context, Poll},
    time::Duration,
};

use slab::Slab;

use crate::driver::{
    Handle,
    backends::DriverBackend,
    ops::{Completion, Op, Operable, State, Submittable},
};

// Newtype around `libc::kevent` for type safety and zero-cost conversion.
//
// If the syscall will block, we need to register an intrest in kqueue to listen for completion
// The op will return the data we need to register that intrest in the driver
//
// The `udata` field is deliberately left as `0` here; the driver fills it
// with the slab index right before the syscall.
#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct Interest(libc::kevent);

impl Interest {
    // Construct a registration interest for the common case.
    //
    // `flags` is usually `EV_ADD | EV_ONESHOT` (recommended for one-shot ops).
    pub const fn new(fd: RawFd, filter: i16, flags: u16) -> Self {
        Self(libc::kevent {
            ident: fd as libc::uintptr_t,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: 0 as *mut libc::c_void, // driver will overwrite
        })
    }

    // Low-level accessor used only by the kqueue driver.
    #[inline]
    pub(crate) fn as_kevent_mut(&mut self) -> &mut libc::kevent {
        &mut self.0
    }
}

// Kqueue is an event notification based system.
// You make the syscall in a non blocking mode, and it will return to you two possiblities -
// 1. The syscall completed and returned you the `Completion` result.
// 2. The syscall will block, in which case you registed a notification and wait
pub(crate) enum Submission {
    Ready(Completion),
    Register(Interest),
}

struct Slot {
    state: State,
    interest: Option<Interest>,
}

pub(crate) struct KqueueBackend {
    kqueue: OwnedFd,
    ops: Slab<Slot>,
    events: Vec<libc::kevent>,
}

impl KqueueBackend {
    pub(crate) fn new() -> Result<Self> {
        let raw_kqueue = unsafe { libc::kqueue() };
        if raw_kqueue < 0 {
            return Err(Error::last_os_error());
        }

        Ok(Self {
            kqueue: unsafe { OwnedFd::from_raw_fd(raw_kqueue) },
            // TODO: Make this configurable later
            ops: Slab::with_capacity(1024),
            events: vec![unsafe { std::mem::zeroed() }; 1024],
        })
    }

    fn delete_interest(kqueue: RawFd, mut interest: Interest) {
        interest.as_kevent_mut().flags = libc::EV_DELETE;

        let kevent = [interest.0];

        let _ = unsafe { libc::kevent(kqueue, kevent.as_ptr(), 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }
}

impl DriverBackend for KqueueBackend {
    fn submit_op<T: Submittable>(&mut self, data: T, handle: Handle) -> Result<Op<T>> {
        let index = self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
        });

        Ok(Op::<T>::new(index, data, handle))
    }

    fn remove_op<T>(&mut self, op: &mut Op<T>) {
        let index = op.index();
        let slot = match self.ops.get_mut(index) {
            Some(val) => val,
            None => {
                // Op already dropped or removed
                return;
            }
        };

        match &slot.state {
            State::Submitted | State::Waiting(..) => {
                // Cancel any registered interest (EV_DELETE is synchronous and safe)
                // The future was dropped while the kernel may still complete the operation.
                // Ask the kernel to cancel it, but keep the slot and data alive until the completion packet arrives.
                if let Some(interest) = slot.interest.take() {
                    Self::delete_interest(self.kqueue.as_raw_fd(), interest);
                }
                slot.state = State::Ignored(Box::new(()));
            }
            State::Completed(..) | State::Ready => {
                // The kernel already completed and no one will poll the future, so the slot can be released immediately.
                self.ops.remove(index);
                let _ = op.take_data();
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    fn poll_op<T: Operable>(&mut self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Result> {
        let index = op.index();

        let Some(slot) = self.ops.get_mut(index) else {
            // This means the op has already being removed. Should not happen in normal circumstances
            unreachable!("invalid operation state")
        };

        // Take the state out so we can freely mutate it and call kevent
        // without overlapping mutable borrows of `self.ops`.
        let current_state = std::mem::replace(&mut slot.state, State::Submitted);

        match current_state {
            State::Ready | State::Submitted => {
                // Kernel says ready (or first poll) → run the non-blocking syscall
                let data = op.data_mut().expect("Op data consumed");

                match data.submit() {
                    Submission::Ready(completion) => {
                        // Synchronous completion
                        let data = op.take_data().expect("Op data consumed");
                        let result = data.complete(completion);
                        self.ops.remove(index);
                        Poll::Ready(result)
                    }

                    Submission::Register(mut interest) => {
                        // Would-block → register and park
                        interest.as_kevent_mut().udata = index as *mut libc::c_void;

                        let kevent = [interest.0];
                        let res = unsafe {
                            libc::kevent(
                                self.kqueue.as_raw_fd(),
                                kevent.as_ptr(),
                                1,
                                std::ptr::null_mut(),
                                0,
                                std::ptr::null(),
                            )
                        };

                        // This means registering the kevent errored out.
                        // In that case, we get the error and mark the op as completed with error
                        if res < 0 {
                            let err = std::io::Error::last_os_error();
                            let data = op.take_data().expect("Op data consumed");
                            let result = data.complete(Completion {
                                result: Err(err),
                                flags: 0,
                            });
                            self.ops.remove(index);
                            return Poll::Ready(result);
                        }

                        slot.interest = Some(interest);
                        slot.state = State::Waiting(cx.waker().clone());
                        Poll::Pending
                    }
                }
            }
            State::Waiting(mut waker) => {
                // Keep waiting for the event, but make sure readiness wakes
                // the currently polling task if the future moved executors.
                if !waker.will_wake(cx.waker()) {
                    waker.clone_from(cx.waker());
                }

                slot.state = State::Waiting(waker);
                Poll::Pending
            }
            // The op has been ignored/cancelled by the caller. It should not be polled again
            State::Ignored(..) | State::Completed(..) => {
                unreachable!("invalid operation")
            }
        }
    }

    fn submit(&mut self) -> Result<()> {
        // kqueue has no batched submission queue — everything is done synchronously in poll_op.
        Ok(())
    }

    fn wait(&mut self) -> Result<usize> {
        let n = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                std::ptr::null(),
                0,
                self.events.as_mut_ptr(),
                self.events.len() as i32,
                std::ptr::null(), // infinite timeout
            )
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(0); // common in reactors — just retry
            }
            return Err(err);
        }

        let n = n as usize;
        // Since the buffer is written to by the kernel, Vec does not know its new length
        // So we set length to the number of events the kernel reported
        unsafe {
            self.events.set_len(n);
        };
        Ok(n)
    }

    fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        let timeout = duration_to_timespec(duration);
        let n = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                std::ptr::null(),
                0,
                self.events.as_mut_ptr(),
                self.events.len() as i32,
                &timeout,
            )
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(err);
        }

        let n = n as usize;
        // Since the buffer is written to by the kernel, Vec does not know its new length
        // So we set length to the number of events the kernel reported
        unsafe {
            self.events.set_len(n);
        };
        Ok(n)
    }

    fn dispatch_completions(&mut self) {
        for event in &self.events {
            let index = event.udata as usize;

            if let Some(slot) = self.ops.get_mut(index) {
                // EV_ONESHOT fired. It is no longer registered in the kernel,
                // so the slot should not remember it as a cancellable interest.
                slot.interest = None;

                let should_drop = slot.state.ready();
                if should_drop {
                    self.ops.remove(index);
                }
            }
            // else: event belongs to a canceled op → ignore (safe)
        }

        // All completions have been processed, so we clear the vec for the next round
        // Note: This does not deallocate the vec, so we still have the existing capacity
        self.events.clear();
    }
}

impl AsRawFd for KqueueBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.kqueue.as_raw_fd()
    }
}

impl Drop for KqueueBackend {
    fn drop(&mut self) {
        // Cancel any still-registered interests synchronously.
        // This is the kqueue equivalent of submitting `AsyncCancel` in io_uring.
        // (EV_DELETE is immediate and safe even if the event has already fired.)
        for (_, slot) in self.ops.iter_mut() {
            if let Some(interest) = slot.interest.take() {
                Self::delete_interest(self.kqueue.as_raw_fd(), interest);
            }
        }

        // We can simply clear the slab now.
        // Unlike io_uring, the kernel no longer owns any resources associated with these indexes after EV_DELETE.
        self.ops.clear();

        // Final sanity check
        debug_assert!(self.ops.is_empty(), "kqueue driver shutdown left ops in the slab");
    }
}

fn duration_to_timespec(duration: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}
