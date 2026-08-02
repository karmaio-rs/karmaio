//! Rustix-backed kqueue driver for macOS and the BSD family.
//!
//! The backend keeps the readiness protocol local to kqueue. Operation futures
//! retain their typed payloads, while this module owns only lifecycle state,
//! readiness interests, and terminal completions.

use std::{
    collections::VecDeque,
    io::{self, Error, Result},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use rustix::{
    buffer::spare_capacity,
    event::{Timespec, kqueue},
    io::{Errno, FdFlags, fcntl_setfd},
};

use crate::driver::ops::{BlockingJob, Completion, Op, OpKey, OpTable};
use crate::driver::{Handle, Wakeup};
use crate::runtime::blocking::BlockingPoolHandle;

/// The readiness direction represented by a kqueue filter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Direction {
    Read,
    Write,
}

/// A small, backend-owned readiness interest.
///
/// The key is deliberately not stored here. The driver adds the current
/// generational key only when it builds the rustix event, making stale-event
/// validation explicit and keeping the operation result independent of the
/// kqueue representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Interest {
    fd: RawFd,
    direction: Direction,
}

impl Interest {
    /// Construct an interest for the given readiness direction. The resulting
    /// value contains no platform event struct.
    pub(crate) const fn new(fd: RawFd, direction: Direction) -> Self {
        Self { fd, direction }
    }

    fn event(self, key: OpKey, flags: kqueue::EventFlags) -> kqueue::Event {
        let filter = match self.direction {
            Direction::Read => kqueue::EventFilter::Read(self.fd),
            Direction::Write => kqueue::EventFilter::Write(self.fd),
        };
        kqueue::Event::new(filter, flags, key.raw() as *mut std::ffi::c_void)
    }
}

/// Results of attempting a kqueue operation on the runtime thread.
pub(crate) enum KqueueAttempt {
    Ready(Completion),
    Register(Interest),
    /// Offload a synchronous operation to the runtime blocking pool.
    Blocking(BlockingJob),
}

/// Backend-local readiness operation protocol.
pub(crate) trait KqueueOperation: 'static {
    type Output;

    /// Attempt the operation without blocking the runtime thread.
    fn attempt(&mut self) -> KqueueAttempt;

    /// Convert a terminal syscall or blocking-pool result into the typed output.
    fn complete(self, completion: Completion) -> Self::Output;
}

enum State {
    Submitted,
    Ready,
    Waiting(Waker),
    Completed(Completion),
    Ignored(Box<dyn IgnoredOp>),
}

trait IgnoredOp: 'static {
    fn cleanup(self: Box<Self>, completion: Completion);
}

impl<T: KqueueOperation + 'static> IgnoredOp for T {
    fn cleanup(self: Box<Self>, completion: Completion) {
        drop(KqueueOperation::complete(*self, completion));
    }
}

impl State {
    fn complete(&mut self, completion: Completion) -> (bool, Option<Waker>) {
        match self {
            State::Submitted | State::Ready => {
                *self = State::Completed(completion);
                (false, None)
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                if let State::Waiting(waker) = old {
                    (false, Some(waker))
                } else {
                    (false, None)
                }
            }
            State::Ignored(_) => {
                if let State::Ignored(payload) = std::mem::replace(self, State::Submitted) {
                    payload.cleanup(completion);
                }
                (true, None)
            }
            // Ignore duplicate readiness/completion notifications. The first
            // terminal result remains available for the future to consume.
            State::Completed(..) => (false, None),
        }
    }

    fn ready(&mut self) -> (bool, Option<Waker>) {
        match self {
            State::Submitted => {
                *self = State::Ready;
                (false, None)
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Ready);
                if let State::Waiting(waker) = old {
                    (false, Some(waker))
                } else {
                    (false, None)
                }
            }
            State::Ignored(..) => (true, None),
            State::Ready | State::Completed(..) => (false, None),
        }
    }
}

struct Slot {
    state: State,
    interest: Option<Interest>,
}

/// A decoded operation readiness event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KqueueEvent {
    key: OpKey,
    fd: RawFd,
    direction: Direction,
    readable: bool,
    writable: bool,
}

/// Low-level kqueue owner: descriptor, reusable event storage, and wakeup.
struct Kqueue {
    fd: Arc<OwnedFd>,
    events: Vec<kqueue::Event>,
    wakeup: KqueueWakeup,
}

impl Kqueue {
    fn new(capacity: usize) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "kqueue event capacity must be greater than zero",
            ));
        }

        let fd = Arc::new(kqueue::kqueue()?);
        fcntl_setfd(fd.as_ref(), FdFlags::CLOEXEC)?;

        let wakeup = KqueueWakeup {
            state: Arc::new(WakeupState {
                fd: Arc::clone(&fd),
                kind: WakeupKind::new()?,
                closed: AtomicBool::new(false),
                notified: AtomicBool::new(false),
            }),
        };

        let queue = Self {
            fd,
            events: Vec::with_capacity(capacity),
            wakeup,
        };
        queue.wakeup.state.kind.register(queue.fd.as_ref())?;
        Ok(queue)
    }

    fn wakeup(&self) -> KqueueWakeup {
        self.wakeup.clone()
    }

    fn close_wakeup(&self) {
        self.wakeup.close();
    }

    fn arm(&self, interest: Interest, key: OpKey) -> io::Result<()> {
        submit_changes(
            self.fd.as_ref(),
            &[interest.event(
                key,
                kqueue::EventFlags::ADD | kqueue::EventFlags::ONESHOT | kqueue::EventFlags::RECEIPT,
            )],
        )
    }

    fn delete(&self, interest: Interest, key: OpKey) -> io::Result<()> {
        submit_changes(
            self.fd.as_ref(),
            &[interest.event(key, kqueue::EventFlags::DELETE | kqueue::EventFlags::RECEIPT)],
        )
    }

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        let deadline = timeout.and_then(|duration| Instant::now().checked_add(duration));

        loop {
            self.events.clear();
            let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let timeout = timeout.and_then(|duration| Timespec::try_from(duration).ok());

            let result = unsafe {
                kqueue::kevent_timespec(
                    self.fd.as_ref(),
                    &[],
                    spare_capacity(&mut self.events),
                    timeout.as_ref(),
                )
            };

            match result {
                Ok(_) => {
                    // A pipe wakeup is one-shot, so drain it and install the
                    // next filter before allowing another notification.
                    self.wakeup.state.notified.store(false, Ordering::SeqCst);
                    self.wakeup.state.kind.reregister(self.fd.as_ref())?;
                    return Ok(self.events.len());
                }
                Err(Errno::INTR) => continue,
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn events(&self) -> impl Iterator<Item = KqueueEvent> + '_ {
        self.events.iter().filter_map(decode_event)
    }
}

impl AsFd for Kqueue {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().as_fd()
    }
}

impl AsRawFd for Kqueue {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for Kqueue {
    fn drop(&mut self) {
        self.wakeup.close();
        let _ = self.wakeup.state.kind.deregister(self.fd.as_ref());
    }
}

/// Cloneable cross-thread wakeup token for the kqueue backend.
#[derive(Clone)]
struct KqueueWakeup {
    state: Arc<WakeupState>,
}

impl KqueueWakeup {
    fn wake(&self) {
        if self.state.closed.load(Ordering::Acquire) {
            return;
        }

        if self
            .state
            .notified
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // A full pipe means a notification is already pending. Other
            // failures are harmless during teardown; Wakeup is infallible.
            let _ = self.state.kind.notify(self.state.fd.as_ref());
        }
    }

    fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
    }
}

struct WakeupState {
    fd: Arc<OwnedFd>,
    kind: WakeupKind,
    closed: AtomicBool,
    notified: AtomicBool,
}

#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
fn readiness_change(fd: RawFd, direction: Direction, flags: kqueue::EventFlags, token: usize) -> kqueue::Event {
    let filter = match direction {
        Direction::Read => kqueue::EventFilter::Read(fd),
        Direction::Write => kqueue::EventFilter::Write(fd),
    };
    kqueue::Event::new(filter, flags, token as *mut std::ffi::c_void)
}

fn submit_changes(fd: &OwnedFd, changes: &[kqueue::Event]) -> io::Result<()> {
    let mut receipts = Vec::with_capacity(changes.len());

    unsafe {
        kqueue::kevent_timespec(fd, changes, spare_capacity(&mut receipts), None)?;
    }

    for receipt in receipts {
        let data = receipt.data();
        if receipt.flags().contains(kqueue::EventFlags::ERROR)
            && data != 0
            && data != Errno::NOENT.raw_os_error() as i64
            && data != Errno::PIPE.raw_os_error() as i64
        {
            return Err(io::Error::from_raw_os_error(data as i32));
        }
    }

    Ok(())
}

fn decode_event(event: &kqueue::Event) -> Option<KqueueEvent> {
    let key = OpKey::from_raw(event.udata() as usize)?;
    let (fd, direction, readable, writable) = match event.filter() {
        kqueue::EventFilter::Read(fd) => (
            fd,
            Direction::Read,
            true,
            event.flags().contains(kqueue::EventFlags::EOF),
        ),
        kqueue::EventFilter::Write(fd) => (fd, Direction::Write, false, true),
        _ => return None,
    };

    Some(KqueueEvent {
        key,
        fd,
        direction,
        readable,
        writable,
    })
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "dragonfly"))]
struct WakeupKind;

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "dragonfly"))]
impl WakeupKind {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    fn register(&self, fd: &OwnedFd) -> io::Result<()> {
        submit_changes(
            fd,
            &[kqueue::Event::new(
                kqueue::EventFilter::User {
                    ident: 0,
                    flags: kqueue::UserFlags::empty(),
                    user_flags: kqueue::UserDefinedFlags::new(0),
                },
                kqueue::EventFlags::ADD | kqueue::EventFlags::RECEIPT | kqueue::EventFlags::CLEAR,
                usize::MAX as *mut std::ffi::c_void,
            )],
        )
    }

    fn reregister(&self, _fd: &OwnedFd) -> io::Result<()> {
        Ok(())
    }

    fn notify(&self, fd: &OwnedFd) -> io::Result<()> {
        submit_changes(
            fd,
            &[kqueue::Event::new(
                kqueue::EventFilter::User {
                    ident: 0,
                    flags: kqueue::UserFlags::TRIGGER,
                    user_flags: kqueue::UserDefinedFlags::new(0),
                },
                kqueue::EventFlags::ADD | kqueue::EventFlags::RECEIPT,
                usize::MAX as *mut std::ffi::c_void,
            )],
        )
    }

    fn deregister(&self, fd: &OwnedFd) -> io::Result<()> {
        submit_changes(
            fd,
            &[kqueue::Event::new(
                kqueue::EventFilter::User {
                    ident: 0,
                    flags: kqueue::UserFlags::empty(),
                    user_flags: kqueue::UserDefinedFlags::new(0),
                },
                kqueue::EventFlags::DELETE | kqueue::EventFlags::RECEIPT,
                usize::MAX as *mut std::ffi::c_void,
            )],
        )
    }
}

#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
struct WakeupKind {
    read: std::os::unix::net::UnixStream,
    write: std::os::unix::net::UnixStream,
}

#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
impl WakeupKind {
    fn new() -> io::Result<Self> {
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(Self { read, write })
    }

    fn register(&self, fd: &OwnedFd) -> io::Result<()> {
        submit_changes(
            fd,
            &[readiness_change(
                self.read.as_raw_fd(),
                Direction::Read,
                kqueue::EventFlags::ADD | kqueue::EventFlags::ONESHOT | kqueue::EventFlags::RECEIPT,
                usize::MAX,
            )],
        )
    }

    fn reregister(&self, fd: &OwnedFd) -> io::Result<()> {
        use io::Read;

        let mut buffer = [0_u8; 64];
        loop {
            match (&self.read).read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }

        self.register(fd)
    }

    fn notify(&self, _fd: &OwnedFd) -> io::Result<()> {
        use io::Write;
        (&self.write).write_all(&[1])
    }

    fn deregister(&self, fd: &OwnedFd) -> io::Result<()> {
        submit_changes(
            fd,
            &[readiness_change(
                self.read.as_raw_fd(),
                Direction::Read,
                kqueue::EventFlags::DELETE | kqueue::EventFlags::RECEIPT,
                usize::MAX,
            )],
        )
    }
}

/// Kqueue backend with a readiness-native lifecycle state machine.
pub(crate) struct KqueueBackend {
    kqueue: Kqueue,
    ops: OpTable<Slot>,
    /// Completions produced by blocking-pool workers (key, result).
    blocking_done: Arc<Mutex<VecDeque<(OpKey, Completion)>>>,
}

impl KqueueBackend {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            kqueue: Kqueue::new(capacity)?,
            ops: OpTable::new(capacity)?,
            blocking_done: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn delete_interest(&self, key: OpKey, interest: Interest) {
        let _ = self.kqueue.delete(interest, key);
    }

    fn push_blocking(&self, key: OpKey, job: BlockingJob, pool: &BlockingPoolHandle, wakeup: &Wakeup) -> Result<()> {
        let done = Arc::clone(&self.blocking_done);
        let wakeup = wakeup.clone();
        pool.try_dispatch(Box::new(move || {
            let completion =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run())).unwrap_or_else(|_| Completion {
                    result: Err(Error::other("blocking operation panicked")),
                });
            done.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((key, completion));
            wakeup.wake();
        }))
    }

    pub(crate) fn submit_op<T: KqueueOperation + 'static>(&mut self, data: T, handle: Handle) -> Result<Op<T>> {
        let key = self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
        })?;

        Ok(Op::<T>::new(key, data, handle))
    }

    pub(crate) fn remove_op<T: KqueueOperation + 'static>(&mut self, op: &mut Op<T>) {
        let key = op.key();
        let Some(slot) = self.ops.get_mut(key) else {
            return;
        };

        match &slot.state {
            State::Submitted => {
                self.ops.remove(key);
                let _ = op.take_data();
            }
            State::Waiting(..) => {
                if let Some(interest) = slot.interest.take() {
                    // EV_DELETE is synchronous. The non-blocking syscall has
                    // not produced a resource yet.
                    self.delete_interest(key, interest);
                    self.ops.remove(key);
                    let _ = op.take_data();
                } else {
                    // A blocking-pool job still owns the operation payload so
                    // late completion can clean up resources it produces.
                    let data = op.take_data().expect("op data missing on detach");
                    slot.state = State::Ignored(Box::new(data));
                }
            }
            State::Completed(_) => {
                let slot = self.ops.remove(key).expect("completed operation disappeared");
                if let State::Completed(completion) = slot.state
                    && let Some(data) = op.take_data()
                {
                    drop(KqueueOperation::complete(data, completion));
                }
            }
            State::Ready => {
                self.ops.remove(key);
                let _ = op.take_data();
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    pub(crate) fn poll_op<T: KqueueOperation + 'static>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        blocking: &BlockingPoolHandle,
        wakeup: &Wakeup,
    ) -> Poll<T::Output> {
        let key = op.key();
        let current_state = {
            let slot = self.ops.get_mut(key).expect("invalid internal operation state");
            std::mem::replace(&mut slot.state, State::Submitted)
        };

        match current_state {
            State::Completed(completion) => {
                let data = op.take_data().expect("op data consumed");
                let result = KqueueOperation::complete(data, completion);
                self.ops.remove(key);
                Poll::Ready(result)
            }
            State::Ready | State::Submitted => {
                let data = op.data_mut().expect("op data consumed");
                match KqueueOperation::attempt(data) {
                    KqueueAttempt::Ready(completion) => {
                        let data = op.take_data().expect("op data consumed");
                        let result = KqueueOperation::complete(data, completion);
                        self.ops.remove(key);
                        Poll::Ready(result)
                    }
                    KqueueAttempt::Register(interest) => {
                        if let Err(error) = self.kqueue.arm(interest, key) {
                            let data = op.take_data().expect("op data consumed");
                            let result = KqueueOperation::complete(data, Completion { result: Err(error) });
                            self.ops.remove(key);
                            return Poll::Ready(result);
                        }

                        let slot = self.ops.get_mut(key).expect("operation removed while registering");
                        slot.interest = Some(interest);
                        slot.state = State::Waiting(cx.waker().clone());
                        Poll::Pending
                    }
                    KqueueAttempt::Blocking(job) => {
                        let slot = self.ops.get_mut(key).expect("operation removed while dispatching");
                        slot.state = State::Waiting(cx.waker().clone());
                        if let Err(error) = self.push_blocking(key, job, blocking, wakeup) {
                            let data = op.take_data().expect("op data consumed");
                            self.ops.remove(key);
                            Poll::Ready(KqueueOperation::complete(data, Completion { result: Err(error) }))
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }
            State::Waiting(mut waker) => {
                if !waker.will_wake(cx.waker()) {
                    waker.clone_from(cx.waker());
                }
                self.ops.get_mut(key).expect("operation removed while waiting").state = State::Waiting(waker);
                Poll::Pending
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    pub(crate) fn submit(&mut self) -> Result<()> {
        // kqueue has no batched submission queue; registration happens in poll_op.
        Ok(())
    }

    pub(crate) fn wait(&mut self) -> Result<usize> {
        self.kqueue.wait(None)
    }

    pub(crate) fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        self.kqueue.wait(Some(duration))
    }

    pub(crate) fn drain_blocking_completions(&mut self) {
        let completions: Vec<_> = {
            let mut pending = self.blocking_done.lock().unwrap_or_else(|e| e.into_inner());
            pending.drain(..).collect()
        };

        let mut wakeups = Vec::new();
        for (key, completion) in completions {
            if let Some(slot) = self.ops.get_mut(key) {
                let (should_drop, waker) = slot.state.complete(completion);
                if let Some(waker) = waker {
                    wakeups.push(waker);
                }
                if should_drop {
                    self.ops.remove(key);
                }
            }
        }
        for waker in wakeups {
            waker.wake();
        }
    }

    pub(crate) fn dispatch_completions(&mut self) -> Result<()> {
        let events: Vec<_> = self.kqueue.events().collect();
        let mut wakeups = Vec::new();

        for event in events {
            // A deleted registration can still be present in the userspace
            // event batch. Validate both generation and descriptor/filter
            // before consuming the operation's current interest.
            if !self.is_current_interest(event.key, event.fd, event.direction) {
                continue;
            }

            self.mark_ready(event.key, event.fd, event.direction, &mut wakeups);

            // EOF on a read filter means the descriptor cannot make useful
            // progress for a pending write either. Wake the independently
            // registered write operation so its syscall can observe EPIPE or
            // the platform's terminal result. Its own descriptor/filter is
            // validated before it is transitioned.
            if event.readable && event.writable {
                let write_key = self.ops.iter().find_map(|(key, slot)| {
                    (slot.interest == Some(Interest::new(event.fd, Direction::Write))).then_some(key)
                });
                if let Some(write_key) = write_key {
                    self.mark_ready(write_key, event.fd, Direction::Write, &mut wakeups);
                }
            }
        }

        // Kqueue's event storage retains capacity while the next wait fills it.
        self.kqueue.events.clear();
        for waker in wakeups {
            waker.wake();
        }
        Ok(())
    }

    fn is_current_interest(&mut self, key: OpKey, fd: RawFd, direction: Direction) -> bool {
        self.ops
            .get_mut(key)
            .and_then(|slot| slot.interest)
            .is_some_and(|interest| interest == Interest::new(fd, direction))
    }

    fn mark_ready(&mut self, key: OpKey, fd: RawFd, direction: Direction, wakeups: &mut Vec<Waker>) {
        let Some(slot) = self.ops.get_mut(key) else {
            return;
        };
        if slot.interest != Some(Interest::new(fd, direction)) {
            return;
        }
        let _ = slot.interest.take();

        let (should_drop, waker) = slot.state.ready();
        if let Some(waker) = waker {
            wakeups.push(waker);
        }
        if should_drop {
            self.ops.remove(key);
        }
    }

    pub(crate) fn create_wakeup(&self) -> Wakeup {
        let wakeup = self.kqueue.wakeup();
        Wakeup::new(move || wakeup.wake())
    }

    pub(crate) fn attach(&self, _fd: RawFd) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Interest {
    fn matches_event(self, event: KqueueEvent) -> bool {
        self.fd == event.fd
            && self.direction == event.direction
            && match self.direction {
                Direction::Read => event.readable,
                Direction::Write => event.writable,
            }
    }
}

impl AsRawFd for KqueueBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.kqueue.as_raw_fd()
    }
}

impl Drop for KqueueBackend {
    fn drop(&mut self) {
        self.kqueue.close_wakeup();

        // Readiness deletion is synchronous, so no kernel-owned operation
        // state survives the table clear. Keep this pass separate from the
        // mutable table borrow to make the ownership boundary obvious.
        let interests: Vec<_> = self
            .ops
            .iter_mut()
            .filter_map(|(key, slot)| slot.interest.take().map(|interest| (key, interest)))
            .collect();
        for (key, interest) in interests {
            self.delete_interest(key, interest);
        }

        // The blocking pool is dropped before the driver by Runtime, so any
        // jobs that completed during pool shutdown are already in this queue.
        // Apply them before removing detached payloads, otherwise a successful
        // open/accept result could be dropped without running its typed cleanup.
        self.drain_blocking_completions();

        let leftovers: Vec<_> = self.ops.iter().map(|(key, _)| key).collect();
        for key in leftovers {
            let Some(slot) = self.ops.remove(key) else {
                continue;
            };
            if let State::Ignored(payload) = slot.state {
                payload.cleanup(Completion {
                    result: Err(Error::other("kqueue backend shut down before operation completion")),
                });
            }
        }

        self.ops.clear();
        debug_assert!(self.ops.is_empty(), "kqueue driver shutdown left ops in the table");
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

    impl KqueueOperation for CleanupMarker {
        type Output = ();

        fn attempt(&mut self) -> KqueueAttempt {
            KqueueAttempt::Ready(Completion { result: Ok(0) })
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
        assert!(!state.complete(Completion { result: Ok(7) }).0);
        assert!(matches!(state, State::Completed(..)));
    }

    #[test]
    fn detached_completion_runs_typed_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut state = State::Ignored(Box::new(CleanupMarker(cleaned.clone())));

        assert!(state.complete(Completion { result: Ok(0) }).0);
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[test]
    fn readiness_wakes_the_current_waiter() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = std::task::Waker::from(Arc::new(WakeMarker(woken.clone())));
        let mut state = State::Waiting(waker);

        let (removed, waker) = state.ready();
        assert!(!removed);
        waker.expect("waiting state should return its waker").wake();
        assert!(woken.load(Ordering::SeqCst));
        assert!(matches!(state, State::Ready));
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    mod kqueue_tests {
        use super::*;
        use rustix::io::{FdFlags, fcntl_getfd};
        use std::{io::Write, os::fd::AsRawFd, os::unix::net::UnixStream};

        fn test_key() -> OpKey {
            OpTable::new(1).unwrap().insert(()).unwrap()
        }

        #[test]
        fn queue_is_close_on_exec() {
            let queue = Kqueue::new(8).expect("create kqueue");
            let flags = fcntl_getfd(&queue).expect("read descriptor flags");
            assert!(flags.contains(FdFlags::CLOEXEC));
        }

        #[test]
        fn readiness_event_round_trips_key_and_descriptor() {
            let mut queue = Kqueue::new(8).expect("create kqueue");
            let (reader, mut writer) = UnixStream::pair().expect("create socket pair");
            reader.set_nonblocking(true).unwrap();
            writer.set_nonblocking(true).unwrap();
            let key = test_key();

            queue
                .arm(Interest::new(reader.as_raw_fd(), Direction::Read), key)
                .unwrap();
            writer.write_all(&[1]).unwrap();
            assert!(queue.wait(Some(Duration::from_secs(1))).unwrap() > 0);
            assert!(
                queue
                    .events()
                    .any(|event| { event.key == key && event.fd == reader.as_raw_fd() && event.readable })
            );
        }

        #[test]
        fn missing_filter_delete_is_benign() {
            let queue = Kqueue::new(8).expect("create kqueue");
            let (reader, _) = UnixStream::pair().unwrap();
            queue
                .delete(Interest::new(reader.as_raw_fd(), Direction::Read), test_key())
                .unwrap();
        }

        #[test]
        fn stale_descriptor_and_filter_events_are_rejected() {
            let interest = Interest::new(17, Direction::Read);
            let key = test_key();

            assert!(!interest.matches_event(KqueueEvent {
                key,
                fd: 18,
                direction: Direction::Read,
                readable: true,
                writable: false,
            }));
            assert!(!interest.matches_event(KqueueEvent {
                key,
                fd: 17,
                direction: Direction::Write,
                readable: false,
                writable: true,
            }));
        }

        #[test]
        fn read_eof_also_releases_pending_write_interest() {
            let mut backend = KqueueBackend::new(8).expect("create backend");
            let read_key = backend
                .ops
                .insert(Slot {
                    state: State::Waiting(std::task::Waker::noop().clone()),
                    interest: Some(Interest::new(42, Direction::Read)),
                })
                .unwrap();
            let write_key = backend
                .ops
                .insert(Slot {
                    state: State::Waiting(std::task::Waker::noop().clone()),
                    interest: Some(Interest::new(42, Direction::Write)),
                })
                .unwrap();

            backend.kqueue.events.push(kqueue::Event::new(
                kqueue::EventFilter::Read(42),
                kqueue::EventFlags::EOF,
                read_key.raw() as *mut std::ffi::c_void,
            ));
            backend.dispatch_completions().unwrap();

            assert!(backend.ops.get_mut(read_key).unwrap().interest.is_none());
            assert!(backend.ops.get_mut(write_key).unwrap().interest.is_none());
            assert!(matches!(backend.ops.get_mut(read_key).unwrap().state, State::Ready));
            assert!(matches!(backend.ops.get_mut(write_key).unwrap().state, State::Ready));
        }

        #[test]
        fn repeated_wakeups_are_coalesced() {
            let mut queue = Kqueue::new(8).expect("create kqueue");
            let wakeup = queue.wakeup();
            wakeup.wake();
            wakeup.wake();
            assert!(queue.wait(Some(Duration::from_secs(1))).unwrap() > 0);
            assert_eq!(queue.events().count(), 0);
            assert_eq!(queue.wait(Some(Duration::ZERO)).unwrap(), 0);
        }

        #[test]
        fn wake_after_close_is_a_no_op() {
            let mut queue = Kqueue::new(8).expect("create kqueue");
            let wakeup = queue.wakeup();
            queue.close_wakeup();
            wakeup.wake();
            assert_eq!(queue.wait(Some(Duration::ZERO)).unwrap(), 0);
        }
    }
}
