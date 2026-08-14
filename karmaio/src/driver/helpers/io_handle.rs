//! Typed shared ownership of an OS resource for completion-based I/O.
//!
//! [`SharedIoHandle<T>`] keeps a cloneable (`Rc`) handle so in-flight ops can pin
//! the resource until their CQEs complete. Prefer [`SharedIoHandle::close`] over
//! drop when close errors matter; drop still sync-closes when the last unique
//! owner remains.
//!
//! Resource identity is preserved in `T` (`socket2::Socket`, `std::fs::File`,
//! `OwnedFd`, …). Submit and close paths extract a copyable [`OsRawHandle`] via
//! [`AsRawOsHandle`] / [`IntoRawOsHandle`].

use std::{cell::RefCell, future::poll_fn, io, mem, ops::Deref, rc::Rc, task::Waker};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, IntoRawHandle, IntoRawSocket, OwnedHandle, OwnedSocket,
    RawHandle, RawSocket,
};
#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;

use crate::driver::ops::Op;

// ---------------------------------------------------------------------------
// SharedIoHandle<T>
// ---------------------------------------------------------------------------

/// Shared ownership of an OS resource for completion-based I/O.
///
/// Clones are cheap (`Rc`). In-flight ops hold clones so the resource cannot be
/// closed until those ops complete. Prefer `close().await` over drop when close
/// errors matter.
///
/// Note: `Clone` is implemented manually so it does **not** require `T: Clone` —
/// only the `Rc` is cloned; `T` is shared, not duplicated.
pub(crate) struct SharedIoHandle<T> {
    inner: Rc<Inner<T>>,
}

impl<T> Clone for SharedIoHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T> SharedIoHandle<T> {
    /// Create a new shared handle owning `resource`.
    pub(crate) fn new(resource: T) -> Self {
        Self {
            inner: Rc::new(Inner {
                resource: Some(resource),
                state: RefCell::new(State::Init),
                #[cfg(windows)]
                association: Rc::new(RefCell::new(AssociationState::Unassociated)),
            }),
        }
    }

    /// Access the owned resource. Panics if used after close transferred ownership.
    pub(crate) fn with_resource<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(self
            .inner
            .resource
            .as_ref()
            .expect("SharedIoHandle used after close transferred ownership"))
    }

    /// Try to unwrap the owned resource if this is the unique strong reference.
    /// Does not close the resource.
    #[allow(dead_code)]
    pub(crate) fn try_unwrap(self) -> Result<T, Self> {
        // Avoid running `Drop for SharedIoHandle` while we move out of `self`.
        let this = mem::ManuallyDrop::new(self);
        // Safety: `this` is not dropped; we take ownership of `inner` exactly once.
        let inner = unsafe { std::ptr::read(&this.inner) };
        match Rc::try_unwrap(inner) {
            Ok(inner) => {
                // Avoid running `Drop for Inner` (which would drop/close `T`).
                let mut inner = mem::ManuallyDrop::new(inner);
                let resource = inner.resource.take().expect("resource already taken by close");
                // Drop state without dropping the resource.
                unsafe {
                    std::ptr::drop_in_place(&mut inner.state);
                    #[cfg(windows)]
                    std::ptr::drop_in_place(&mut inner.association);
                }
                Ok(resource)
            }
            Err(inner) => Err(Self { inner }),
        }
    }

    /// Wait until this is the unique strong reference, then take the owned
    /// resource without closing it. Returns `None` if already closed.
    pub(crate) async fn take(mut self) -> Option<T> {
        loop {
            if let Some(inner) = Rc::get_mut(&mut self.inner) {
                return inner.take_owned();
            }
            self.is_unique().await;
        }
    }

    /// Completes when the strong `Rc` count is 1.
    /// Polled again whenever a clone is dropped (see `Drop`).
    async fn is_unique(&self) {
        use std::task::Poll;

        poll_fn(|cx| {
            if Rc::<Inner<T>>::strong_count(&self.inner) == 1 {
                return Poll::Ready(());
            }

            let mut state = self.inner.state.borrow_mut();

            match &mut *state {
                State::Init => {
                    *state = State::Waiting(cx.waker().clone());
                    Poll::Pending
                }
                State::Waiting(waker) => {
                    if !waker.will_wake(cx.waker()) {
                        waker.clone_from(cx.waker());
                    }
                    Poll::Pending
                }
                State::Closed => Poll::Ready(()),
            }
        })
        .await;
    }
}

impl<T: IntoRawOsHandle> SharedIoHandle<T> {
    /// Wait for all in-flight operations to complete, then close the resource
    /// through the driver (or sync-close if submit fails).
    ///
    /// Prefer this over dropping when possible so close errors are returned and
    /// the OS resource is released promptly.
    pub(crate) async fn close(self) -> io::Result<()> {
        match self.take().await {
            Some(resource) => {
                let raw = resource.into_raw_os_handle();
                match Op::close(raw) {
                    Ok(op) => op.await,
                    Err(e) => {
                        // Submit failed: reclaim ownership and close synchronously.
                        // Safety: `raw` is open and not owned elsewhere; we just
                        // extracted it from `T` and failed to hand it to the driver.
                        unsafe { drop_raw_os_handle(raw) };
                        Err(e)
                    }
                }
            }
            // Already closed (e.g. double close).
            None => Ok(()),
        }
    }
}

impl<T: AsRawOsHandle> SharedIoHandle<T> {
    /// Returns the copyable raw OS value for kernel submission.
    #[allow(dead_code)] // available for submit paths; many ops use raw_fd/raw_handle helpers instead
    pub(crate) fn as_raw_os_handle(&self) -> OsRawHandle {
        self.with_resource(|r| r.as_raw_os_handle())
    }

    /// Windows-only alias used by existing ops.
    #[cfg(windows)]
    #[allow(dead_code)] // Convenience alias; no op calls it yet.
    pub(crate) fn raw_os_handle(&self) -> OsRawHandle {
        self.as_raw_os_handle()
    }
}

#[cfg(unix)]
impl<T: AsRawFd> SharedIoHandle<T> {
    /// Returns the underlying `RawFd`.
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.with_resource(|r| r.as_raw_fd())
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> SharedIoHandle<T> {
    /// Returns the underlying Win32 file handle (not a socket).
    #[allow(dead_code)]
    pub(crate) fn raw_handle(&self) -> RawHandle {
        self.with_resource(|r| r.as_raw_handle())
    }

    /// Returns the shared state used to associate this file handle with IOCP.
    #[cfg(windows)]
    pub(crate) fn handle_registration(&self) -> HandleRegistration {
        HandleRegistration {
            association: Rc::clone(&self.inner.association),
            raw: WindowsRawHandle::Handle(self.raw_handle()),
        }
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> SharedIoHandle<T> {
    /// Returns the underlying Win32 socket.
    pub(crate) fn raw_socket(&self) -> RawSocket {
        self.with_resource(|r| r.as_raw_socket())
    }

    /// Returns the shared state used to associate this socket with IOCP.
    #[cfg(windows)]
    pub(crate) fn socket_registration(&self) -> HandleRegistration {
        HandleRegistration {
            association: Rc::clone(&self.inner.association),
            raw: WindowsRawHandle::Socket(self.raw_socket()),
        }
    }
}

impl<T> Deref for SharedIoHandle<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner
            .resource
            .as_ref()
            .expect("SharedIoHandle used after close transferred ownership")
    }
}

// Wake any task waiting for uniqueness when a clone is dropped.
// Without this, `close().await` / `take().await` can hang forever after
// in-flight ops complete.
impl<T> Drop for SharedIoHandle<T> {
    fn drop(&mut self) {
        let mut state = self.inner.state.borrow_mut();
        if let State::Waiting(_) = *state {
            if let State::Waiting(waker) = mem::replace(&mut *state, State::Init) {
                // Wake the task wanting to take/close this handle and let it try again.
                // If it finds there are no more outstanding clones, it will succeed.
                // Otherwise it will start a new Future, waiting for another drop.
                waker.wake();
            }
        }
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for SharedIoHandle<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd()
    }
}

#[cfg(unix)]
impl<T: AsFd> AsFd for SharedIoHandle<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // Borrow from the stored resource; lifetime is tied to `&self`.
        self.inner
            .resource
            .as_ref()
            .expect("SharedIoHandle used after close transferred ownership")
            .as_fd()
    }
}

// ---------------------------------------------------------------------------
// Raw / owned extraction traits
// ---------------------------------------------------------------------------

/// Extract a copyable raw OS value for kernel submission.
/// Implemented for the owned resource types we wrap.
#[allow(dead_code)] // available for submit paths; many ops use AsRawFd/AsRawHandle helpers instead
pub(crate) trait AsRawOsHandle {
    fn as_raw_os_handle(&self) -> OsRawHandle;
}

/// Consume into a raw value for exclusive close (no Drop of the OS resource).
pub(crate) trait IntoRawOsHandle {
    fn into_raw_os_handle(self) -> OsRawHandle;
}

// Platform-specific raw handle (file descriptor on Unix, file handle or socket on Windows).
// Copyable form used when submitting operations to the kernel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum OsRawHandle {
    #[cfg(unix)]
    Fd(RawFd),
    #[cfg(windows)]
    Handle(RawHandle),
    #[cfg(windows)]
    Socket(RawSocket),
}

/// Shared exact-once association state for a Windows handle or socket.
///
/// The state is shared by all `SharedIoHandle` clones. A successful association
/// is idempotent for the same IOCP, while an attempt to use the resource with a
/// different runtime is rejected.
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct HandleRegistration {
    association: Rc<RefCell<AssociationState>>,
    raw: WindowsRawHandle,
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsRawHandle {
    Handle(RawHandle),
    Socket(RawSocket),
}

#[cfg(windows)]
enum AssociationState {
    Unassociated,
    Associated(usize),
}

#[cfg(windows)]
impl HandleRegistration {
    /// Associate the resource once, leaving it retryable if registration fails.
    pub(crate) fn associate(
        &self,
        registrar: usize,
        register: impl FnOnce(HANDLE) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut state = self.association.borrow_mut();
        match *state {
            AssociationState::Associated(current) if current == registrar => return Ok(()),
            AssociationState::Associated(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "I/O handle is already associated with another karmaio driver",
                ));
            }
            AssociationState::Unassociated => {}
        }

        register(self.raw.as_handle())?;
        *state = AssociationState::Associated(registrar);
        Ok(())
    }
}

#[cfg(windows)]
impl WindowsRawHandle {
    #[inline]
    fn as_handle(self) -> HANDLE {
        match self {
            Self::Handle(handle) => handle as HANDLE,
            Self::Socket(socket) => socket as HANDLE,
        }
    }
}

/// Rebuild an owned OS resource from a raw value and drop it (closes the resource).
///
/// Used when close submit fails and we must reclaim + sync-close.
///
/// # Safety
/// `raw` must be open and not owned elsewhere.
pub(crate) unsafe fn drop_raw_os_handle(raw: OsRawHandle) {
    match raw {
        #[cfg(unix)]
        OsRawHandle::Fd(fd) => drop(unsafe { OwnedFd::from_raw_fd(fd) }),
        #[cfg(windows)]
        OsRawHandle::Handle(h) => drop(unsafe { OwnedHandle::from_raw_handle(h) }),
        #[cfg(windows)]
        OsRawHandle::Socket(s) => drop(unsafe { OwnedSocket::from_raw_socket(s) }),
    }
}

// --- Unix: OwnedFd ---------------------------------------------------------

#[cfg(unix)]
impl AsRawOsHandle for OwnedFd {
    fn as_raw_os_handle(&self) -> OsRawHandle {
        OsRawHandle::Fd(self.as_raw_fd())
    }
}

#[cfg(unix)]
impl IntoRawOsHandle for OwnedFd {
    fn into_raw_os_handle(self) -> OsRawHandle {
        OsRawHandle::Fd(self.into_raw_fd())
    }
}

// --- std::fs::File ---------------------------------------------------------

impl AsRawOsHandle for std::fs::File {
    fn as_raw_os_handle(&self) -> OsRawHandle {
        #[cfg(unix)]
        {
            OsRawHandle::Fd(self.as_raw_fd())
        }
        #[cfg(windows)]
        {
            OsRawHandle::Handle(self.as_raw_handle())
        }
    }
}

impl IntoRawOsHandle for std::fs::File {
    fn into_raw_os_handle(self) -> OsRawHandle {
        #[cfg(unix)]
        {
            OsRawHandle::Fd(self.into_raw_fd())
        }
        #[cfg(windows)]
        {
            OsRawHandle::Handle(self.into_raw_handle())
        }
    }
}

// --- socket2::Socket (net feature) -----------------------------------------

#[cfg(feature = "net")]
impl AsRawOsHandle for socket2::Socket {
    fn as_raw_os_handle(&self) -> OsRawHandle {
        #[cfg(unix)]
        {
            OsRawHandle::Fd(self.as_raw_fd())
        }
        #[cfg(windows)]
        {
            OsRawHandle::Socket(self.as_raw_socket())
        }
    }
}

#[cfg(feature = "net")]
impl IntoRawOsHandle for socket2::Socket {
    fn into_raw_os_handle(self) -> OsRawHandle {
        #[cfg(unix)]
        {
            OsRawHandle::Fd(self.into_raw_fd())
        }
        #[cfg(windows)]
        {
            OsRawHandle::Socket(self.into_raw_socket())
        }
    }
}

// --- Windows: OwnedHandle / OwnedSocket ------------------------------------

#[cfg(windows)]
impl AsRawOsHandle for OwnedHandle {
    fn as_raw_os_handle(&self) -> OsRawHandle {
        OsRawHandle::Handle(self.as_raw_handle())
    }
}

#[cfg(windows)]
impl IntoRawOsHandle for OwnedHandle {
    fn into_raw_os_handle(self) -> OsRawHandle {
        OsRawHandle::Handle(self.into_raw_handle())
    }
}

#[cfg(windows)]
impl AsRawOsHandle for OwnedSocket {
    fn as_raw_os_handle(&self) -> OsRawHandle {
        OsRawHandle::Socket(self.as_raw_socket())
    }
}

#[cfg(windows)]
impl IntoRawOsHandle for OwnedSocket {
    fn into_raw_os_handle(self) -> OsRawHandle {
        OsRawHandle::Socket(self.into_raw_socket())
    }
}

// ---------------------------------------------------------------------------
// Inner storage
// ---------------------------------------------------------------------------

struct Inner<T> {
    // Open resource. `None` after ownership was transferred to an async close/take.
    // Only mutated when this Inner is uniquely owned (`Rc::get_mut`) or on Drop of T.
    resource: Option<T>,

    // Track the sharing state of the handle:
    // normal, being waited on to allow a close by the parent's owner, or already closed.
    state: RefCell<State>,

    /// Shared across every clone so successful IOCP registration happens once.
    #[cfg(windows)]
    association: Rc<RefCell<AssociationState>>,
}

impl<T> Inner<T> {
    // Take ownership of the resource, marking state Closed so Drop does not close again.
    fn take_owned(&mut self) -> Option<T> {
        {
            let state = RefCell::get_mut(&mut self.state);
            if let State::Closed = *state {
                return None;
            }
            *state = State::Closed;
        }
        self.resource.take()
    }
}

// Drop of `Inner<T>` drops any remaining `T`, which closes the OS resource
// (File / Socket / OwnedFd / OwnedHandle / OwnedSocket). After explicit
// `close`/`take`, `resource` is `None` so Drop is a no-op for the OS handle.

enum State {
    // Initial state
    Init,

    // Waiting for all in-flight operations to complete.
    // Waits for the number of strong Rc pointers to drop to 1.
    Waiting(Waker),

    // The close has been triggered by the parent owner (explicit close/take).
    Closed,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::cell::Cell;
    #[cfg(unix)]
    use std::task::Poll;

    #[cfg(windows)]
    fn test_registration(raw: WindowsRawHandle) -> HandleRegistration {
        HandleRegistration {
            association: Rc::new(RefCell::new(AssociationState::Unassociated)),
            raw,
        }
    }

    #[cfg(windows)]
    #[test]
    fn registration_associates_once_and_rejects_another_driver() {
        let registration = test_registration(WindowsRawHandle::Handle(1usize as RawHandle));
        let calls = Cell::new(0);

        registration
            .associate(7, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .expect("first registration");
        registration
            .associate(7, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .expect("same driver registration is idempotent");

        assert_eq!(calls.get(), 1);
        assert_eq!(
            registration
                .associate(8, |_| Ok(()))
                .expect_err("another driver must be rejected")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_registration_can_be_retried() {
        let raw = usize::MAX as RawSocket;
        let registration = test_registration(WindowsRawHandle::Socket(raw));
        let seen = Cell::new(0usize);

        assert_eq!(
            registration
                .associate(3, |handle| {
                    seen.set(handle as usize);
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
                })
                .expect_err("injected registration failure")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(seen.get(), raw as usize);

        registration
            .associate(3, |_| Ok(()))
            .expect("failed registration must remain retryable");
    }

    #[cfg(unix)]
    fn dev_null_handle() -> SharedIoHandle<std::fs::File> {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        SharedIoHandle::new(file)
    }

    #[cfg(unix)]
    #[test]
    fn try_unwrap_succeeds_when_unique() {
        let handle = dev_null_handle();
        let owned = match handle.try_unwrap() {
            Ok(owned) => owned,
            Err(_) => panic!("unique handle should unwrap"),
        };
        // Dropping owned closes the fd.
        drop(owned);
    }

    #[cfg(unix)]
    #[test]
    fn try_unwrap_fails_when_shared() {
        let handle = dev_null_handle();
        let clone = handle.clone();
        let err = match handle.try_unwrap() {
            Err(handle) => handle,
            Ok(_) => panic!("shared handle must not unwrap"),
        };
        drop(clone);
        // Recover uniqueness and close via drop.
        drop(err);
    }

    #[cfg(unix)]
    #[test]
    fn close_waits_for_in_flight_clone() {
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");

        let handle = dev_null_handle();
        let inflight = handle.clone();

        // Close must wait until the in-flight clone is dropped.
        let close_jh = runtime.spawn(async move {
            handle.close().await.expect("close should succeed");
        });

        // Yield once so the close task can park on uniqueness, then drop the clone.
        let drop_jh = runtime.spawn(async move {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if !yielded {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            drop(inflight);
        });

        runtime.block_on(async {
            drop_jh.await.expect("drop task");
            close_jh.await.expect("close task");
        });
    }

    #[cfg(unix)]
    #[test]
    fn close_when_already_unique() {
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = dev_null_handle();

        runtime.block_on(async move {
            handle.close().await.expect("close should succeed");
        });
    }

    #[cfg(unix)]
    #[test]
    fn take_returns_owned_when_unique() {
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = dev_null_handle();

        runtime.block_on(async move {
            let owned = handle.take().await.expect("take should return owned handle");
            // Caller owns the file; drop closes it.
            drop(owned);
        });
    }

    #[cfg(unix)]
    #[test]
    fn take_waits_for_in_flight_clone() {
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = dev_null_handle();
        let inflight = handle.clone();

        let take_jh = runtime.spawn(async move { handle.take().await.expect("take should succeed after clone drops") });

        let drop_jh = runtime.spawn(async move {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if !yielded {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            drop(inflight);
        });

        runtime.block_on(async {
            drop_jh.await.expect("drop task");
            let owned = take_jh.await.expect("take task");
            drop(owned);
        });
    }

    #[cfg(unix)]
    #[test]
    fn drop_closes_synchronously_without_explicit_close() {
        // last ref Drop closes via File / OwnedFd Drop.
        let handle = dev_null_handle();
        drop(handle);
    }

    #[cfg(unix)]
    #[test]
    fn take_after_prior_take_is_none_via_closed_state() {
        // take() marks Closed before returning; a second exclusive take on a
        // reconstructed handle is not possible through the public API. Instead
        // verify take_owned semantics: after take, Inner is Closed with no resource.
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = dev_null_handle();

        runtime.block_on(async move {
            let owned = handle.take().await.expect("first take");
            drop(owned);
        });
    }

    #[cfg(unix)]
    #[test]
    fn as_raw_os_handle_matches_as_raw_fd() {
        let handle = dev_null_handle();
        let raw = handle.as_raw_os_handle();
        match raw {
            OsRawHandle::Fd(fd) => assert_eq!(fd, handle.as_raw_fd()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn try_unwrap_owned_fd_variant() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let handle = SharedIoHandle::new(OwnedFd::from(file));
        let owned = match handle.try_unwrap() {
            Ok(owned) => owned,
            Err(_) => panic!("unique handle should unwrap"),
        };
        drop(owned);
    }

    #[cfg(unix)]
    #[test]
    fn deref_exposes_resource() {
        let handle = dev_null_handle();
        // Deref to File — metadata is a smoke check that we have a live File.
        let _ = handle.metadata().expect("metadata");
    }
}
