use std::{
    collections::{HashMap, VecDeque},
    io::{Error, Result},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError, WAIT_TIMEOUT},
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::IO::{
        CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
        PostQueuedCompletionStatus,
    },
    System::WindowsProgramming::FILE_SKIP_SET_EVENT_ON_HANDLE,
};

use crate::driver::ops::{OpKey, OpTable};
use crate::driver::{
    Handle, Wakeup,
    ops::{BlockingJob, Completion, Op},
};
use crate::runtime::blocking::BlockingPoolHandle;

// Stable allocation passed to Windows for one overlapped operation.
//
// `raw` must stay first: completion entries give us back `*mut OVERLAPPED`,
// which we cast back to this header to recover the slab index.
#[repr(C)]
struct OverlappedHeader {
    raw: OVERLAPPED,
    key: OpKey,
}

// Owned OVERLAPPED state for a submitted IOCP operation.
//
// The memory behind this value is boxed so moving `Interest` between an op and
// the driver does not invalidate the pointer already handed to Windows.
pub(crate) struct Interest {
    overlapped: Box<OverlappedHeader>,
    handle: HANDLE,
}

impl Interest {
    // Creates zeroed overlapped state for an operation on `handle`.
    //
    // The driver fills the slab index after the operation has been accepted.
    pub(crate) fn new(handle: RawHandle) -> Self {
        Self {
            overlapped: Box::new(OverlappedHeader {
                raw: OVERLAPPED::default(),
                key: OpKey::INVALID,
            }),
            handle: handle as HANDLE,
        }
    }

    // Returns the pointer to pass to Windows overlapped APIs.
    #[inline]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut OVERLAPPED {
        &mut self.overlapped.raw
    }

    #[inline]
    fn as_ptr(&self) -> *const OVERLAPPED {
        &self.overlapped.raw
    }

    #[inline]
    fn handle(&self) -> HANDLE {
        self.handle
    }

    #[inline]
    fn set_key(&mut self, key: OpKey) {
        self.overlapped.key = key;
    }

    #[inline]
    unsafe fn key_from_raw(overlapped: *mut OVERLAPPED) -> OpKey {
        // SAFETY: `Interest::as_mut_ptr` returns a pointer to the first field of
        // `OverlappedHeader`, and that boxed header is kept alive in the slab
        // until the completion packet has been processed.
        unsafe { (*(overlapped as *mut OverlappedHeader)).key }
    }
}

pub(crate) enum IocpSubmission {
    Ready(Completion),
    Pending(Interest),
    /// Offload a Send closure to the runtime blocking pool.
    Blocking(BlockingJob),
}

/// Build a normal one-shot IOCP submission.
///
/// Implementations must keep every buffer, length field, and other memory
/// referenced by the returned overlapped request alive until completion.
pub(crate) trait IocpSubmit {
    fn submit(&mut self) -> IocpSubmission;
}

pub(crate) trait IocpComplete {
    type Result;

    fn complete(self, completion: Completion) -> Self::Result;
}

/// Backend-local IOCP operation protocol.
///
/// Implementations must keep all memory referenced by the returned overlapped
/// request valid until Windows delivers its terminal completion packet.
pub(crate) trait IocpOperation: IocpSubmit + IocpComplete<Result = Self::Output> {
    type Output;
}

impl<T: IocpSubmit + IocpComplete> IocpOperation for T {
    type Output = T::Result;
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

impl<T: IocpOperation + 'static> IgnoredOp for T {
    fn cleanup(self: Box<Self>, completion: Completion) {
        drop(IocpComplete::complete(*self, completion));
    }
}

struct Detached;

impl IgnoredOp for Detached {
    fn cleanup(self: Box<Self>, _completion: Completion) {}
}

impl State {
    fn complete(&mut self, completion: Completion) -> (bool, Option<Waker>) {
        match self {
            State::Submitted => {
                *self = State::Completed(completion);
                (false, None)
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                if let State::Waiting(waker) = old {
                    return (false, Some(waker));
                }
                (false, None)
            }
            State::Ignored(_) => {
                if let State::Ignored(payload) = std::mem::replace(self, State::Submitted) {
                    payload.cleanup(completion);
                }
                (true, None)
            }
            // Ignore duplicate terminal packets. The first completion remains
            // available for the future to consume.
            State::Completed(..) => (false, None),
        }
    }
}

// A tracked operation slot in the driver slab.
//
// The slab index is the logical op id. Pending IOCP operations also keep an
// `Interest`, whose boxed OVERLAPPED stores this same index so completions can
// find the slot without looking at the completion key.
//
// After detach, the typed op payload lives in `State::Ignored` so a late
// completion packet can cleanup produced resources (accept sockets, open handles).
struct Slot {
    state: State,
    interest: Option<Interest>,
    /// Set when `submit()` returned `Blocking`; dispatched on first poll.
    blocking_job: Option<BlockingJob>,
}

/// The completion port that receives kernel completion packets.
struct CompletionPort {
    port: OwnedHandle,
}

impl CompletionPort {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };
        if handle.is_null() {
            return Err(Error::last_os_error());
        }

        Ok(Self {
            // SAFETY: `CreateIoCompletionPort` returned a valid owned handle.
            port: unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) },
        })
    }

    fn add_handle(&self, handle: RawHandle, completion_key: usize) -> Result<()> {
        let result =
            unsafe { CreateIoCompletionPort(handle as HANDLE, self.port.as_raw_handle() as HANDLE, completion_key, 0) };

        if result.is_null() {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn get_many(&self, entries: &mut Vec<OVERLAPPED_ENTRY>, timeout: Option<Duration>) -> Result<usize> {
        let mut count = 0;
        let result = unsafe {
            GetQueuedCompletionStatusEx(
                self.port.as_raw_handle() as HANDLE,
                entries.as_mut_ptr(),
                // Ensures that the writable capacity is always within bounds
                entries.capacity().min(u32::MAX as usize) as u32,
                &mut count,
                duration_millis(timeout),
                0,
            )
        };

        if result == 0 {
            let err = Error::last_os_error();

            if err.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                return Ok(0);
            }

            Err(err)
        } else {
            // Since the buffer is written to by the kernel, Vec does not know its new length
            // So we set length to the entries kernel reported to us
            unsafe {
                entries.set_len(count as usize);
            };
            Ok(count as usize)
        }
    }
}

impl AsRawHandle for CompletionPort {
    fn as_raw_handle(&self) -> RawHandle {
        self.port.as_raw_handle()
    }
}

/// Shared completion-port wakeup state. The port remains owned by this Arc
/// while remote wakeup closures are still alive, and the closed flag prevents
/// writes after backend shutdown.
struct IocpWakeup {
    port: Arc<CompletionPort>,
    closed: std::sync::atomic::AtomicBool,
}

impl IocpWakeup {
    fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::Release);
    }

    fn wake(&self) {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }

        let _ = unsafe { PostQueuedCompletionStatus(self.port.as_raw_handle() as HANDLE, 0, 0, std::ptr::null_mut()) };
    }
}

pub(crate) struct IocpBackend {
    // Completion port shared by all handles registered with this backend.
    port: Arc<CompletionPort>,
    wakeup: Arc<IocpWakeup>,
    // Per-op state keyed by a generational operation identity.
    ops: OpTable<Slot>,
    // Validate completion pointers before reading the embedded operation key.
    pending_by_overlapped: HashMap<usize, OpKey>,
    // Reused batch buffer for completion packets.
    // Since this is a buffer that is filled by the kernel over ffi, we need to be follow some rules -
    // 1. We will not resize this vec after initialization
    // 2. We will have to manually manage the length of the buffer during -
    //    a. Reading completions from kernel
    //    b. Processing completions
    entries: Vec<OVERLAPPED_ENTRY>,
    /// Completions produced by blocking-pool workers (index, result).
    blocking_done: Arc<Mutex<VecDeque<(OpKey, Completion)>>>,
}

impl IocpBackend {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let port = Arc::new(CompletionPort::new()?);
        let wakeup = Arc::new(IocpWakeup {
            port: Arc::clone(&port),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        Ok(Self {
            port,
            wakeup,
            ops: OpTable::new(capacity)?,
            pending_by_overlapped: HashMap::with_capacity(capacity),
            entries: vec![unsafe { std::mem::zeroed() }; capacity],
            blocking_done: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn push_blocking(&self, key: OpKey, job: BlockingJob, pool: &BlockingPoolHandle, wakeup: &Wakeup) -> Result<()> {
        let done = Arc::clone(&self.blocking_done);
        let wakeup = wakeup.clone();
        pool.try_dispatch(Box::new(move || {
            let completion = panic::catch_unwind(AssertUnwindSafe(|| job.run())).unwrap_or_else(|_| Completion {
                result: Err(Error::other("blocking operation panicked")),
            });
            done.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((key, completion));
            wakeup.wake();
        }))
    }
}

impl IocpBackend {
    pub(crate) fn submit_op<T: IocpOperation + 'static>(&mut self, mut data: T, handle: Handle) -> Result<Op<T>> {
        let key = self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
            blocking_job: None,
        })?;

        match IocpSubmit::submit(&mut data) {
            IocpSubmission::Ready(completion) => {
                // The operation finished before it needed IOCP. Store the
                // completion so the first poll resolves the future.
                self.ops.get_mut(key).expect("inserted IOCP operation missing").state = State::Completed(completion);
            }
            IocpSubmission::Pending(mut interest) => {
                // The handle must be attached to the IOCP before submitting.
                // This is now enforced by the Attacher type at handle creation.
                //
                // The kernel owns this OVERLAPPED until completion. Stamp the
                // slab index into the stable allocation before storing it.
                interest.set_key(key);
                let address = interest.as_ptr() as usize;
                if self.pending_by_overlapped.insert(address, key).is_some() {
                    let _ = self.ops.remove(key);
                    return Err(Error::new(
                        std::io::ErrorKind::Other,
                        "duplicate IOCP OVERLAPPED address",
                    ));
                }
                self.ops.get_mut(key).expect("inserted IOCP operation missing").interest = Some(interest);
            }
            IocpSubmission::Blocking(job) => {
                // Dispatch on first poll when waker + pool/wakeup are available.
                self.ops
                    .get_mut(key)
                    .expect("inserted IOCP operation missing")
                    .blocking_job = Some(job);
            }
        }

        Ok(Op::<T>::new(key, data, handle))
    }

    pub(crate) fn remove_op<T: IocpOperation + 'static>(&mut self, op: &mut Op<T>) {
        let key = op.key();
        let Some(slot) = self.ops.get_mut(key) else {
            // Op already dropped or removed.
            return;
        };

        match &slot.state {
            State::Submitted | State::Waiting(..) => {
                // Blocking job not yet handed to the pool — free immediately.
                if slot.blocking_job.take().is_some() {
                    self.ops.remove(key);
                    let _ = op.take_data();
                    return;
                }

                // Overlapped path: ask Windows to cancel, keep payload until
                // the completion packet arrives so we can cleanup accept/open.
                if let Some(interest) = slot.interest.as_ref() {
                    let result = unsafe { CancelIoEx(interest.handle(), interest.as_ptr()) };

                    if result == 0 {
                        let err = Error::last_os_error();
                        debug_assert_eq!(
                            err.raw_os_error(),
                            Some(ERROR_NOT_FOUND as i32),
                            "CancelIoEx failed: {err}"
                        );
                    }
                }

                let data = op.take_data().expect("op data missing on detach");
                slot.state = State::Ignored(Box::new(data));
            }
            State::Completed(_) => {
                // Completion already dequeued but never polled — cleanup now.
                // (Also covers Accept Drop closing pre-allocated sockets when
                // complete moves them out; errors leave Drop as the backstop.)
                let slot = self.ops.remove(key).expect("completed operation disappeared");
                if let State::Completed(completion) = slot.state {
                    if let Some(data) = op.take_data() {
                        drop(IocpComplete::complete(data, completion));
                    }
                }
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
        }
    }

    pub(crate) fn poll_op<T: IocpOperation + 'static>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        blocking: &BlockingPoolHandle,
        wakeup: &Wakeup,
    ) -> Poll<T::Output> {
        let key = op.key();
        let slot = self.ops.get_mut(key).expect("invalid internal state");

        // First poll of a Blocking submission: hand off to the pool.
        if let Some(job) = slot.blocking_job.take() {
            slot.state = State::Waiting(cx.waker().clone());
            if let Err(error) = self.push_blocking(key, job, blocking, wakeup) {
                let data = op.take_data().expect("Op data consumed");
                self.ops.remove(key);
                return Poll::Ready(IocpComplete::complete(data, Completion { result: Err(error) }));
            }
            return Poll::Pending;
        }

        let state = &mut self.ops.get_mut(key).expect("invalid internal state").state;

        match state {
            State::Submitted => {
                // First poll after submission: remember this task's waker and
                // let the IOCP completion path wake it.
                *state = State::Waiting(cx.waker().clone());
                Poll::Pending
            }
            State::Waiting(waker) => {
                // The task moved or was re-polled with a different waker.
                // Replace it so completion wakes the current task.
                if !waker.will_wake(cx.waker()) {
                    *state = State::Waiting(cx.waker().clone());
                }
                Poll::Pending
            }
            State::Completed(_) => match self
                .ops
                .remove(op.key())
                .expect("completed operation disappeared")
                .state
            {
                // Completion was already dispatched. Consume op data exactly
                // once and let the op-specific code decode the CQ result.
                State::Completed(completion) => {
                    Poll::Ready(IocpComplete::complete(op.take_data().unwrap(), completion))
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
        // IOCP operations are submitted by the individual overlapped syscalls.
        Ok(())
    }

    pub(crate) fn wait(&mut self) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, None)?;
        Ok(num_entries)
    }

    pub(crate) fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, Some(duration))?;
        Ok(num_entries)
    }

    pub(crate) fn drain_blocking_completions(&mut self) {
        // Called by the runtime after wait* (see Runtime::block_on).
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
        let mut wakeups = Vec::new();
        for entry in &self.entries {
            let overlapped = entry.lpOverlapped;

            if overlapped.is_null() {
                // This can be a wakeup packet posted via PostQueuedCompletionStatus
                // from another thread (remote task scheduling), or other injected packets.
                // Nothing to do; the wait has woken up.
                continue;
            }

            let address = overlapped as usize;
            let Some(expected_key) = self.pending_by_overlapped.remove(&address) else {
                // Ignore foreign or duplicate packets without dereferencing an
                // untrusted OVERLAPPED pointer.
                continue;
            };
            let key = unsafe { Interest::key_from_raw(overlapped) };
            if key != expected_key {
                self.pending_by_overlapped.insert(address, expected_key);
                debug_assert_eq!(key, expected_key, "IOCP operation key changed while pending");
                continue;
            }
            let status = (entry.Internal as u32) as i32;
            let result = if status >= 0 {
                Ok(entry.dwNumberOfBytesTransferred)
            } else {
                let err = unsafe { RtlNtStatusToDosError(status) };
                Err(Error::from_raw_os_error(err as i32))
            };

            if let Some(slot) = self.ops.get_mut(key) {
                // Once the packet is dequeued, Windows no longer references
                // the OVERLAPPED allocation.
                slot.interest = None;

                // `State::complete` handles both waking a live waiter and
                // reporting whether an ignored op can now be dropped.
                let (should_drop, waker) = slot.state.complete(Completion { result });
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

        // All entires have been processed, so we clear the vec for the next round
        // Note: This does not deallocate the vec, so we still have the existing capacity
        self.entries.clear();
        Ok(())
    }

    pub(crate) fn create_wakeup(&self) -> Wakeup {
        let wakeup = Arc::clone(&self.wakeup);
        crate::driver::Wakeup::new(move || wakeup.wake())
    }

    pub(crate) fn attach(&self, handle: RawHandle) -> Result<()> {
        self.port.add_handle(handle, 0)?;

        // Avoid event objects, but retain completion-port packets for synchronous
        // overlapped success. The operation protocol represents such requests as
        // `Pending` and therefore relies on the packet being delivered.
        unsafe {
            let _ = SetFileCompletionNotificationModes(handle as HANDLE, FILE_SKIP_SET_EVENT_ON_HANDLE as u8);
        }

        Ok(())
    }
}

impl Drop for IocpBackend {
    fn drop(&mut self) {
        self.wakeup.close();
        for (_, slot) in self.ops.iter_mut() {
            match slot.state {
                State::Submitted | State::Waiting(..) => {
                    // In-flight ops must be canceled and then drained from the
                    // completion port before their OVERLAPPED memory can go.
                    // Preserve an existing Ignored cleanup when present; otherwise
                    // install Detached (Op::drop should already have moved data).
                    if let Some(interest) = slot.interest.as_ref() {
                        let _ = unsafe { CancelIoEx(interest.handle(), interest.as_ptr()) };
                    }
                    if !matches!(slot.state, State::Ignored(..)) {
                        slot.state = State::Ignored(Box::new(Detached));
                    }
                }
                State::Ignored(..) => {
                    // Already canceled by the future drop path; the drain loop
                    // below will wait for its completion packet.
                }
                State::Completed(..) => {
                    // Completion has already been dispatched, so no kernel
                    // reference remains.
                }
            }
        }

        while self
            .ops
            .iter()
            .any(|(_, slot)| matches!(slot.state, State::Ignored(..)))
        {
            if self.wait().is_err() {
                continue;
            }

            self.drain_blocking_completions();
            self.dispatch_completions()
                .expect("internal error while draining IOCP completions");
        }

        self.ops.clear();
    }
}

#[inline]
fn duration_millis(duration: Option<Duration>) -> u32 {
    match duration {
        Some(duration) => {
            let millis = duration
                .checked_add(Duration::from_nanos(999_999))
                .unwrap_or(duration)
                .as_millis();

            millis.min(u32::MAX as u128) as u32
        }
        None => u32::MAX,
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

    impl IocpSubmit for CleanupMarker {
        fn submit(&mut self) -> IocpSubmission {
            IocpSubmission::Ready(Completion { result: Ok(0) })
        }
    }

    impl IocpComplete for CleanupMarker {
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
    fn completion_wakes_the_current_waiter() {
        let woken = Arc::new(AtomicBool::new(false));
        let waker = std::task::Waker::from(Arc::new(WakeMarker(woken.clone())));
        let mut state = State::Waiting(waker);

        assert!(!state.complete(Completion { result: Ok(0) }).0);
        assert!(woken.load(Ordering::SeqCst));
    }
}
