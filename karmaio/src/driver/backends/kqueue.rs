use std::{
    collections::VecDeque,
    io::{Error, Result},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use slab::Slab;

use crate::driver::{
    Handle, Wakeup,
    backends::DriverBackend,
    ops::{BlockingJob, Completable, Completion, Op, Operable, State},
};
use crate::runtime::blocking::BlockingPoolHandle;

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
// 3. The work must run on the blocking pool (`Blocking`).
pub(crate) enum Submission {
    Ready(Completion),
    Register(Interest),
    /// Offload a Send closure to the runtime blocking pool.
    Blocking(BlockingJob),
}

struct Slot {
    state: State,
    interest: Option<Interest>,
}

pub(crate) struct KqueueBackend {
    kqueue: OwnedFd,
    ops: Slab<Slot>,
    events: Vec<libc::kevent>,
    /// Completions produced by blocking-pool workers (index, result).
    blocking_done: Arc<Mutex<VecDeque<(usize, Completion)>>>,
}

/// Special ident/udata used for cross-thread wakeups via EVFILT_USER.
/// Chosen to not collide with slab indices (which start small).
const WAKEUP_IDENT: libc::uintptr_t = libc::uintptr_t::MAX;
const WAKEUP_UDATA: *mut libc::c_void = libc::uintptr_t::MAX as *mut libc::c_void;

impl KqueueBackend {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let raw_kqueue = unsafe { libc::kqueue() };
        if raw_kqueue < 0 {
            return Err(Error::last_os_error());
        }

        let backend = Self {
            kqueue: unsafe { OwnedFd::from_raw_fd(raw_kqueue) },
            // `capacity` is configurable via the runtime builder's driver capacity.
            ops: Slab::with_capacity(capacity),
            events: vec![unsafe { std::mem::zeroed() }; capacity],
            blocking_done: Arc::new(Mutex::new(VecDeque::new())),
        };

        // Register a user event (EVFILT_USER) that can be triggered from any
        // thread to wake a blocking kevent() call. This enables the remote task
        // queue to promptly wake the runtime without a fixed timeout.
        let ev = libc::kevent {
            ident: WAKEUP_IDENT,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: WAKEUP_UDATA,
        };
        let _ = unsafe { libc::kevent(raw_kqueue, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };

        Ok(backend)
    }

    fn delete_interest(kqueue: RawFd, mut interest: Interest) {
        interest.as_kevent_mut().flags = libc::EV_DELETE;

        let kevent = [interest.0];

        let _ = unsafe { libc::kevent(kqueue, kevent.as_ptr(), 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    fn push_blocking(&self, index: usize, job: BlockingJob, pool: &BlockingPoolHandle, wakeup: &Wakeup) {
        let done = Arc::clone(&self.blocking_done);
        let wakeup = wakeup.clone();
        pool.dispatch(move || {
            let completion = job.run();
            done.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((index, completion));
            wakeup.wake();
        });
    }
}

impl DriverBackend for KqueueBackend {
    fn submit_op<T: Operable>(&mut self, data: T, handle: Handle) -> Result<Op<T>> {
        let index = self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
        });

        Ok(Op::<T>::new(index, data, handle))
    }

    fn remove_op<T: Completable + 'static>(&mut self, op: &mut Op<T>) {
        let index = op.index();
        let slot = match self.ops.get_mut(index) {
            Some(val) => val,
            None => {
                // Op already dropped or removed
                return;
            }
        };

        match &slot.state {
            State::Submitted => {
                // Never polled — no in-flight work and no produced resource.
                self.ops.remove(index);
                let _ = op.take_data();
            }
            State::Waiting(..) => {
                if let Some(interest) = slot.interest.take() {
                    // Register-path cancel: EV_DELETE is synchronous. The
                    // non-blocking syscall has not produced a resource yet.
                    Self::delete_interest(self.kqueue.as_raw_fd(), interest);
                    self.ops.remove(index);
                    let _ = op.take_data();
                } else {
                    // Blocking-pool job still running. Keep payload so a late
                    // completion can cleanup produced FDs (e.g. open).
                    let data = op.take_data().expect("op data missing on detach");
                    slot.state = State::Ignored(Box::new(data));
                }
            }
            State::Completed(_) => {
                // Completion already stored but never polled — cleanup now.
                let slot = self.ops.remove(index);
                if let State::Completed(completion) = slot.state {
                    if let Some(data) = op.take_data() {
                        drop(data.complete(completion));
                    }
                }
            }
            State::Ready => {
                // Ready to re-issue syscall; nothing produced yet.
                self.ops.remove(index);
                let _ = op.take_data();
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    fn poll_op<T: Operable>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        blocking: &BlockingPoolHandle,
        wakeup: &Wakeup,
    ) -> Poll<T::Result> {
        let index = op.index();

        let Some(slot) = self.ops.get_mut(index) else {
            // This means the op has already being removed. Should not happen in normal circumstances
            unreachable!("invalid operation state")
        };

        // Take the state out so we can freely mutate it and call kevent
        // without overlapping mutable borrows of `self.ops`.
        let current_state = std::mem::replace(&mut slot.state, State::Submitted);

        match current_state {
            State::Completed(completion) => {
                let data = op.take_data().expect("Op data consumed");
                let result = data.complete(completion);
                self.ops.remove(index);
                Poll::Ready(result)
            }
            State::Ready | State::Submitted => {
                // Kernel says ready (or first poll) → run the non-blocking syscall
                // or dispatch blocking work to the pool.
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

                    Submission::Blocking(job) => {
                        slot.state = State::Waiting(cx.waker().clone());
                        self.push_blocking(index, job, blocking, wakeup);
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
            State::Ignored(..) => {
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
                // Use capacity, not len. `dispatch_completions` clears the vec (len = 0) after each round;
                // kevent with nevents=0 returns immediately and would busy-spin.
                self.events.capacity() as i32,
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
                // Use capacity, not len. `dispatch_completions` clears the vec (len = 0) after each round;
                // kevent with nevents=0 returns immediately and would busy-spin.
                self.events.capacity() as i32,
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

    fn drain_blocking_completions(&mut self) {
        // Called by the runtime after wait* (see Runtime::block_on).
        let mut pending = self.blocking_done.lock().unwrap_or_else(|e| e.into_inner());
        while let Some((index, completion)) = pending.pop_front() {
            if let Some(slot) = self.ops.get_mut(index) {
                let should_drop = slot.state.complete(completion);
                if should_drop {
                    self.ops.remove(index);
                }
            }
        }
    }

    fn dispatch_completions(&mut self) {
        for event in &self.events {
            // Cross-thread wakeup (EVFILT_USER) — not an I/O op.
            if event.filter == libc::EVFILT_USER {
                continue;
            }

            let index = event.udata as usize;

            if let Some(slot) = self.ops.get_mut(index) {
                // Only Register-path ops install interest. Blocking-pool ops leave
                // interest as None; a wake packet must never call `ready()` on them
                // (especially after they are already `Completed`).
                let Some(_interest) = slot.interest.take() else {
                    continue;
                };

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

    fn create_wakeup(&self) -> crate::driver::Wakeup {
        let kq = self.kqueue.as_raw_fd();
        crate::driver::Wakeup::new(move || {
            // Perform the EVFILT_USER trigger using the captured raw fd.
            // This is safe to call from any thread.
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            ev.ident = WAKEUP_IDENT;
            ev.filter = libc::EVFILT_USER;
            ev.fflags = libc::NOTE_TRIGGER;
            let _ = unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        })
    }

    fn attach(&self, _fd: RawFd) -> Result<()> {
        // No-op on kqueue: handles don't need explicit registration.
        Ok(())
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
