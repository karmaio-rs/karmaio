use std::{
    collections::HashMap,
    io::{Error, Result},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError, WAIT_TIMEOUT,
    },
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::IO::{
        CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
        PostQueuedCompletionStatus,
    },
    System::WindowsProgramming::FILE_SKIP_SET_EVENT_ON_HANDLE,
};

use crate::driver::helpers::io_handle::HandleRegistration;
use crate::driver::ops::{
    BlockingCompletionGuard, BlockingCompletionQueue, BlockingJob, Completion, DeferredAction, Op, OpKey, OpTable,
};
use crate::driver::{Handle, Wakeup};
use crate::runtime::blocking::BlockingPoolHandle;

// Monotonic identity for an IOCP backend. Using an ID instead of the completion
// port address avoids treating a resource as registered with a new runtime if
// the allocator happens to reuse the old port's address.
static NEXT_REGISTRAR: AtomicUsize = AtomicUsize::new(1);

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

#[inline]
fn cancel_error_is_already_complete(error: &Error) -> bool {
    error.raw_os_error() == Some(ERROR_NOT_FOUND as i32)
}

fn cancel_pending(interest: &Interest) {
    let result = unsafe { CancelIoEx(interest.handle(), interest.as_ptr()) };

    if result == 0 {
        let error = Error::last_os_error();
        if cancel_error_is_already_complete(&error) {
            // The request already completed or its terminal packet is queued.
            // Keep the slot until that packet is dispatched.
        } else {
            // Cancellation is best effort. The detached operation remains in
            // the backend slot, including its OVERLAPPED and payload, so a
            // non-benign failure cannot turn into a use-after-free. The
            // eventual completion packet is still the ownership boundary.
            return;
        }
    }
}

/// Backend-local IOCP operation protocol.
///
/// Implementations must keep all memory referenced by the returned overlapped
/// request valid until Windows delivers its terminal completion packet.
///
/// # Lifecycle
///
/// [`IocpOperation::submit`] is called from `IocpBackend::submit_op`. A
/// `Pending` result parks the OVERLAPPED in the slab until the completion
/// packet arrives. A `Blocking` result is dispatched on first `poll_op`.
/// Typed `complete` runs outside the driver's backend borrow.
///
/// # Safety
///
/// Implementations must ensure that the returned pending submission keeps its
/// `OVERLAPPED`, buffers, lengths, and any other kernel-visible storage valid
/// until the terminal completion packet is dispatched.
pub(crate) unsafe trait IocpOperation: 'static {
    type Output;

    /// Start the operation (invoked during `submit_op`). A pending submission
    /// owns its stable `OVERLAPPED` until Windows delivers the terminal packet.
    fn submit(&mut self) -> IocpSubmission;

    /// Convert a terminal IOCP packet into the operation's typed result.
    fn complete(self, completion: Completion) -> Self::Output;

    /// Request cancellation of a pending operation after its future detaches.
    ///
    /// The default uses `CancelIoEx`, which is correct for the regular
    /// overlapped operations in this crate. An operation with additional
    /// Windows-specific cancellation requirements can override this hook while
    /// keeping ownership of its payload in the backend's detached slot.
    fn cancel(&mut self, interest: &Interest) {
        cancel_pending(interest);
    }
}

enum State {
    Submitted,
    Waiting(Waker),
    Completed(Completion),
    /// Future dropped. Payload retained until the completion packet.
    Detached(Box<dyn IgnoredOp>),
    /// Cancel requested while the future may still be observed.
    Canceling {
        observer: CancelObserver,
    },
}

enum CancelObserver {
    Pending,
    Waiting(Waker),
    Detached(Box<dyn IgnoredOp>),
}

trait IgnoredOp: 'static {
    fn cleanup(self: Box<Self>, completion: Completion);
}

impl<T: IocpOperation + 'static> IgnoredOp for T {
    fn cleanup(self: Box<Self>, completion: Completion) {
        drop(IocpOperation::complete(*self, completion));
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
            State::Detached(_) => {
                if let State::Detached(payload) = std::mem::replace(self, State::Submitted) {
                    let action = DeferredAction::new(move || payload.cleanup(completion));
                    return (true, None, Some(action));
                }
                (true, None, None)
            }
            State::Canceling { .. } => {
                let old = std::mem::replace(self, State::Submitted);
                match old {
                    State::Canceling {
                        observer: CancelObserver::Pending,
                    } => {
                        *self = State::Completed(completion);
                        (false, None, None)
                    }
                    State::Canceling {
                        observer: CancelObserver::Waiting(waker),
                    } => {
                        *self = State::Completed(completion);
                        (false, Some(waker), None)
                    }
                    State::Canceling {
                        observer: CancelObserver::Detached(payload),
                    } => {
                        let action = DeferredAction::new(move || payload.cleanup(completion));
                        (true, None, Some(action))
                    }
                    _ => unreachable!("canceling replace mismatch"),
                }
            }
            // Ignore duplicate terminal packets. The first completion remains
            // available for the future to consume.
            State::Completed(..) => (false, None, None),
        }
    }
}

// A tracked operation slot in the driver slab.
//
// The slab index is the logical op id. Pending IOCP operations also keep an
// `Interest`, whose boxed OVERLAPPED stores this same index so completions can
// find the slot without looking at the completion key.
//
// After detach, the typed op payload lives in `State::Detached` so a late
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

/// Cloneable IOCP association capability owned by a runtime handle.
///
/// Keeping this separate from [`IocpBackend`] lets an operation-created
/// resource attach while the backend is already mutably borrowed to decode a
/// completion. The association call therefore does not re-enter the backend's
/// `RefCell`.
#[derive(Clone)]
pub(crate) struct IocpAssociation {
    port: Arc<CompletionPort>,
    registrar: usize,
}

impl IocpAssociation {
    pub(crate) fn associate(&self, registration: &HandleRegistration) -> Result<()> {
        registration.associate(self.registrar, |handle| {
            self.port.add_handle(handle, 0)?;

            // Avoid event objects, but retain completion-port packets for
            // synchronous overlapped success.
            unsafe {
                let _ = SetFileCompletionNotificationModes(handle as HANDLE, FILE_SKIP_SET_EVENT_ON_HANDLE as u8);
            }

            Ok(())
        })
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
    association: IocpAssociation,
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
    blocking_done: BlockingCompletionQueue,
    shutting_down: bool,
}

impl IocpBackend {
    pub(crate) fn new(config: crate::driver::DriverConfig) -> Result<Self> {
        let capacity = config.capacity;
        let port = Arc::new(CompletionPort::new()?);
        let registrar = NEXT_REGISTRAR.fetch_add(1, Ordering::Relaxed);
        let association = IocpAssociation {
            port: Arc::clone(&port),
            registrar,
        };
        let wakeup = Arc::new(IocpWakeup {
            port: Arc::clone(&port),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        Ok(Self {
            port,
            association,
            wakeup,
            ops: OpTable::new(capacity)?,
            pending_by_overlapped: HashMap::with_capacity(capacity),
            entries: vec![unsafe { std::mem::zeroed() }; capacity],
            blocking_done: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            shutting_down: false,
        })
    }

    fn push_blocking(&self, key: OpKey, job: BlockingJob, pool: &BlockingPoolHandle, wakeup: &Wakeup) -> Result<()> {
        let done = Arc::clone(&self.blocking_done);
        let guard = BlockingCompletionGuard::new(key, done, wakeup.clone());
        // Mandatory: the syscall's side effect (e.g. closing a handle) must run
        // even if the runtime shuts down before the pool picks the job up.
        pool.try_dispatch_mandatory(Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| job.run()))
                .unwrap_or_else(|_| Completion::new(Err(Error::other("blocking operation panicked"))));
            guard.complete(result);
        }))
    }
}

impl IocpBackend {
    /// Submit a oneshot operation.
    ///
    /// On failure the error is paired with the operation payload so callers
    /// that own buffers can recover them; the kernel never observed the op.
    pub(crate) fn submit_op<T: IocpOperation + 'static>(
        &mut self,
        data: T,
        handle: Handle,
    ) -> std::result::Result<Op<T>, (Error, T)> {
        if self.shutting_down {
            return Err((
                Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "IOCP backend is shutting down",
                ),
                data,
            ));
        }

        // Stabilize the operation before `submit` derives buffer or OVERLAPPED
        // pointers. Moving the returned future only moves this box.
        let mut data = Box::new(data);

        let key = match self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
            blocking_job: None,
        }) {
            Ok(key) => key,
            Err(error) => return Err((error, *data)),
        };

        match IocpOperation::submit(&mut *data) {
            IocpSubmission::Ready(completion) => {
                // The operation finished before it needed IOCP. Store the
                // completion so the first poll resolves the future.
                self.ops.get_mut(key).expect("inserted IOCP operation missing").state = State::Completed(completion);
            }
            IocpSubmission::Pending(mut interest) => {
                // The kernel owns this OVERLAPPED until completion. Stamp the
                // slab index into the stable allocation before storing it.
                interest.set_key(key);
                let address = interest.as_ptr() as usize;
                if self.pending_by_overlapped.insert(address, key).is_some() {
                    let _ = self.ops.remove(key);
                    return Err((
                        Error::new(
                            std::io::ErrorKind::Other,
                            "duplicate IOCP OVERLAPPED address",
                        ),
                        *data,
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

    pub(crate) fn remove_op<T: IocpOperation + 'static>(&mut self, op: &mut Op<T>) -> Option<Completion> {
        let key = op.key();
        let Some(slot) = self.ops.get_mut(key) else {
            // Op already dropped or removed.
            return None;
        };

        match &slot.state {
            State::Submitted | State::Waiting(..) => {
                // Blocking job not yet handed to the pool — free immediately.
                if slot.blocking_job.take().is_some() {
                    self.ops.remove(key);
                    let _ = op.take_data();
                    return None;
                }

                // Overlapped path: ask Windows to cancel, keep payload until
                // the completion packet arrives so we can cleanup accept/open.
                let mut data = op.take_data().expect("op data missing on detach");
                if let Some(interest) = slot.interest.as_ref() {
                    IocpOperation::cancel(&mut *data, interest);
                }
                slot.state = State::Detached(data);
                None
            }
            State::Canceling {
                observer: CancelObserver::Pending | CancelObserver::Waiting(_),
            } => {
                let data = op.take_data().expect("op data missing on cancel detach");
                slot.state = State::Canceling {
                    observer: CancelObserver::Detached(data),
                };
                None
            }
            State::Completed(_) => {
                // Completion already dequeued but never polled. Return it so the
                // driver can run typed complete after releasing the backend borrow.
                let slot = self.ops.remove(key).expect("completed operation disappeared");
                match slot.state {
                    State::Completed(completion) => Some(completion),
                    _ => None,
                }
            }
            State::Detached(..)
            | State::Canceling {
                observer: CancelObserver::Detached(..),
            } => unreachable!("invalid operation state"),
        }
    }

    /// Request eager cancellation of a submitted oneshot operation.
    ///
    /// Uses `CancelIoEx` on the exact overlapped structure. The observing
    /// future stays pending until the completion packet arrives.
    pub(crate) fn cancel_op(&mut self, key: OpKey) {
        let Some(slot) = self.ops.get_mut(key) else {
            return;
        };
        match std::mem::replace(&mut slot.state, State::Submitted) {
            State::Submitted => {
                if slot.blocking_job.take().is_some() {
                    slot.state = State::Completed(Completion::new(Err(Error::from_raw_os_error(
                        ERROR_OPERATION_ABORTED as i32,
                    ))));
                    return;
                }
                if let Some(interest) = slot.interest.as_ref() {
                    cancel_pending(interest);
                }
                slot.state = State::Canceling {
                    observer: CancelObserver::Pending,
                };
            }
            State::Waiting(waker) => {
                if let Some(interest) = slot.interest.as_ref() {
                    cancel_pending(interest);
                }
                slot.state = State::Canceling {
                    observer: CancelObserver::Waiting(waker),
                };
            }
            State::Detached(payload) => {
                if let Some(interest) = slot.interest.as_ref() {
                    cancel_pending(interest);
                }
                slot.state = State::Canceling {
                    observer: CancelObserver::Detached(payload),
                };
            }
            other @ (State::Canceling { .. } | State::Completed(_)) => {
                slot.state = other;
            }
        }
    }

    pub(crate) fn poll_op<T: IocpOperation + 'static>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        blocking: &BlockingPoolHandle,
        wakeup: &Wakeup,
    ) -> Poll<Completion> {
        let key = op.key();
        let slot = self.ops.get_mut(key).expect("invalid internal state");

        // First poll of a Blocking submission: hand off to the pool.
        if let Some(job) = slot.blocking_job.take() {
            slot.state = State::Waiting(cx.waker().clone());
            if let Err(error) = self.push_blocking(key, job, blocking, wakeup) {
                self.ops.remove(key);
                return Poll::Ready(Completion::new(Err(error)));
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
            State::Canceling {
                observer: observer @ CancelObserver::Pending,
            } => {
                *observer = CancelObserver::Waiting(cx.waker().clone());
                Poll::Pending
            }
            State::Canceling {
                observer: CancelObserver::Waiting(waker),
                ..
            } => {
                if !waker.will_wake(cx.waker()) {
                    *waker = cx.waker().clone();
                }
                Poll::Pending
            }
            State::Completed(_) => match self
                .ops
                .remove(op.key())
                .expect("completed operation disappeared")
                .state
            {
                State::Completed(completion) => Poll::Ready(completion),
                _ => unreachable!("invalid operation"),
            },
            State::Detached(..)
            | State::Canceling {
                observer: CancelObserver::Detached(..),
            } => {
                unreachable!("invalid operation")
            }
        }
    }

    pub(crate) fn wait(&mut self) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, None)?;
        Ok(num_entries)
    }

    pub(crate) fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, Some(duration))?;
        Ok(num_entries)
    }

    pub(crate) fn drain_blocking_completions(&mut self) -> Vec<DeferredAction> {
        // Called by the runtime after wait* (see Runtime::block_on).
        let completions: Vec<_> = {
            let mut pending = self.blocking_done.lock().unwrap_or_else(|e| e.into_inner());
            pending.drain(..).collect()
        };
        let mut deferred = Vec::new();
        for (key, completion) in completions {
            if let Some(slot) = self.ops.get_mut(key) {
                let (should_drop, waker, cleanup) = slot.state.complete(completion);
                if let Some(waker) = waker {
                    deferred.push(DeferredAction::new(move || waker.wake()));
                }
                if let Some(cleanup) = cleanup {
                    deferred.push(cleanup);
                }
                if should_drop {
                    self.ops.remove(key);
                }
            }
        }
        deferred
    }

    pub(crate) fn dispatch_completions(&mut self) -> Result<Vec<DeferredAction>> {
        let mut deferred = Vec::new();
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
                let (should_drop, waker, cleanup) = slot.state.complete(Completion::new(result));
                if let Some(waker) = waker {
                    deferred.push(DeferredAction::new(move || waker.wake()));
                }
                if let Some(cleanup) = cleanup {
                    deferred.push(cleanup);
                }

                if should_drop {
                    self.ops.remove(key);
                }
            }
        }

        // All entires have been processed, so we clear the vec for the next round
        // Note: This does not deallocate the vec, so we still have the existing capacity
        self.entries.clear();
        Ok(deferred)
    }

    pub(crate) fn create_wakeup(&self) -> Wakeup {
        let wakeup = Arc::clone(&self.wakeup);
        crate::driver::Wakeup::new(move || wakeup.wake())
    }

    pub(crate) fn association(&self) -> IocpAssociation {
        self.association.clone()
    }
}

impl IocpBackend {
    /// Stop accepting work, cancel pending overlapped requests, and drain the
    /// completion port until every kernel-owned operation is released.
    ///
    /// Runtime teardown calls this explicitly after the blocking pool has
    /// joined. `Drop` remains as a final backstop for standalone backend use.
    pub(crate) fn shutdown(&mut self) -> Vec<DeferredAction> {
        if self.shutting_down {
            return Vec::new();
        }
        self.shutting_down = true;
        self.wakeup.close();
        for (_, slot) in self.ops.iter_mut() {
            match std::mem::replace(&mut slot.state, State::Submitted) {
                State::Submitted | State::Waiting(..) => {
                    if let Some(interest) = slot.interest.as_ref() {
                        cancel_pending(interest);
                    }
                    slot.state = State::Detached(Box::new(Detached));
                }
                State::Canceling {
                    observer: CancelObserver::Pending | CancelObserver::Waiting(_),
                } => {
                    if let Some(interest) = slot.interest.as_ref() {
                        cancel_pending(interest);
                    }
                    slot.state = State::Canceling {
                        observer: CancelObserver::Detached(Box::new(Detached)),
                    };
                }
                other @ (State::Detached(..)
                | State::Canceling {
                    observer: CancelObserver::Detached(..),
                }
                | State::Completed(..)) => {
                    slot.state = other;
                }
            }
        }

        while self.ops.iter().any(|(_, slot)| {
            matches!(
                slot.state,
                State::Detached(..)
                    | State::Canceling {
                        observer: CancelObserver::Detached(..),
                    }
            )
        }) {
            if self.wait().is_err() {
                continue;
            }

            DeferredAction::run_all(self.drain_blocking_completions());
            DeferredAction::run_all(
                self.dispatch_completions()
                    .expect("internal error while draining IOCP completions"),
            );
        }

        self.ops.clear();
        Vec::new()
    }
}

impl Drop for IocpBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
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

    unsafe impl IocpOperation for CleanupMarker {
        type Output = ();

        fn submit(&mut self) -> IocpSubmission {
            IocpSubmission::Ready(Completion::new(Ok(0)))
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
        let mut state = State::Detached(Box::new(CleanupMarker(cleaned.clone())));

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
    fn only_not_found_is_a_benign_cancel_race() {
        use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;

        assert!(cancel_error_is_already_complete(&Error::from_raw_os_error(
            ERROR_NOT_FOUND as i32
        )));
        assert!(!cancel_error_is_already_complete(&Error::from_raw_os_error(
            ERROR_INVALID_HANDLE as i32
        )));
    }
}
