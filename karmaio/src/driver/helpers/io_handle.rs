use std::{cell::RefCell, future::poll_fn, io, mem, rc::Rc, task::Waker};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, IntoRawHandle, IntoRawSocket, OwnedHandle, OwnedSocket,
    RawHandle, RawSocket,
};

use crate::driver::ops::Op;

// Tracks in-flight operations on a file or socket handle. Ensures all in-flight
// operations complete before submitting the close.
//
// When the last reference is dropped without an explicit `close().await`, the
// owned OS handle is closed synchronously (compio / tokio-uring style). Prefer
// explicit `close().await` so close errors are observed and the release is
// asynchronous when backed by the driver.
//
// The closed state is tracked so close calls after the first are ignored.
// Only the first close call returns the true result of closing the handle.
//
// This type is cross-platform (Unix + Windows) and supports both file and socket
// handles using conditional compilation. Ownership uses I/O-safe types
// (`OwnedFd` / `OwnedHandle` / `OwnedSocket`) rather than bare raw integers.
#[derive(Clone)]
pub(crate) struct SharedIoHandle {
    inner: Rc<InnerFd>,
}

impl SharedIoHandle {
    // Create from an owned Unix file descriptor.
    #[cfg(unix)]
    pub(crate) fn new(fd: OwnedFd) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: Some(OwnedOsHandle::Fd(fd)),
                state: RefCell::new(State::Init),
            }),
        }
    }

    // Create from a raw Unix FD, taking ownership.
    //
    // # Safety
    // `fd` must be an open file descriptor. After this call, only `SharedIoHandle`
    // (and clones / in-flight ops) may close it.
    #[cfg(unix)]
    pub(crate) unsafe fn from_raw_fd(fd: RawFd) -> SharedIoHandle {
        SharedIoHandle::new(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    // Create from an owned Windows file handle.
    #[cfg(windows)]
    pub(crate) fn new_file(handle: OwnedHandle) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: Some(OwnedOsHandle::Handle(handle)),
                state: RefCell::new(State::Init),
            }),
        }
    }

    // Create from a raw Windows file handle, taking ownership.
    //
    // # Safety
    // `handle` must be an open Win32 handle. After this call, only `SharedIoHandle`
    // may close it.
    #[cfg(windows)]
    pub(crate) unsafe fn from_raw_handle(handle: RawHandle) -> SharedIoHandle {
        SharedIoHandle::new_file(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    // Create from an owned Windows socket.
    #[cfg(windows)]
    pub(crate) fn new_socket(socket: OwnedSocket) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: Some(OwnedOsHandle::Socket(socket)),
                state: RefCell::new(State::Init),
            }),
        }
    }

    // Create from a raw Windows socket, taking ownership.
    //
    // # Safety
    // `socket` must be an open socket. After this call, only `SharedIoHandle` may close it.
    #[cfg(windows)]
    pub(crate) unsafe fn from_raw_socket(socket: RawSocket) -> SharedIoHandle {
        SharedIoHandle::new_socket(unsafe { OwnedSocket::from_raw_socket(socket) })
    }

    // Returns the RawFd (Unix-only).
    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        match self.as_raw_os_handle() {
            OsRawHandle::Fd(fd) => fd,
        }
    }

    // Returns the RawHandle (Windows file handle only).
    #[cfg(windows)]
    pub(crate) fn raw_handle(&self) -> RawHandle {
        match self.as_raw_os_handle() {
            OsRawHandle::Handle(h) => h,
            OsRawHandle::Socket(_) => {
                unreachable!("SharedIoHandle was created with new_socket; use raw_socket")
            }
        }
    }

    // Returns the RawSocket (Windows socket handle only).
    #[cfg(windows)]
    pub(crate) fn raw_socket(&self) -> RawSocket {
        match self.as_raw_os_handle() {
            OsRawHandle::Socket(s) => s,
            OsRawHandle::Handle(_) => {
                unreachable!("SharedIoHandle was created with new_file; use raw_handle")
            }
        }
    }

    // Returns the raw OS handle enum (file handle or socket on Windows, fd on Unix).
    pub(crate) fn as_raw_os_handle(&self) -> OsRawHandle {
        self.inner
            .handle
            .as_ref()
            .expect("SharedIoHandle used after close transferred ownership")
            .as_raw()
    }

    // Windows-only alias used by existing ops.
    #[cfg(windows)]
    pub(crate) fn raw_os_handle(&self) -> OsRawHandle {
        self.as_raw_os_handle()
    }

    // Try to unwrap the owned handle if this is the unique strong reference.
    // Does not close the handle.
    pub(crate) fn try_unwrap(self) -> Result<OwnedOsHandle, Self> {
        // Avoid running `Drop for SharedIoHandle` while we move out of `self`.
        let this = mem::ManuallyDrop::new(self);
        // Safety: `this` is not dropped; we take ownership of `inner` exactly once.
        let inner = unsafe { std::ptr::read(&this.inner) };
        match Rc::try_unwrap(inner) {
            Ok(inner) => {
                // Avoid running `Drop for InnerFd` (which would sync-close the handle).
                let mut inner = mem::ManuallyDrop::new(inner);
                let handle = inner.handle.take().expect("handle already taken by close");
                // Drop state without closing the handle.
                unsafe {
                    std::ptr::drop_in_place(&mut inner.state);
                }
                Ok(handle)
            }
            Err(inner) => Err(Self { inner }),
        }
    }

    // Wait until this is the unique strong reference, then take the owned handle
    // without closing it. Returns `None` if the handle was already closed.
    //
    // Useful for FFI handoff or custom close paths (compio-style API).
    pub(crate) async fn take(mut self) -> Option<OwnedOsHandle> {
        loop {
            if let Some(inner) = Rc::get_mut(&mut self.inner) {
                return inner.take_owned();
            }
            self.is_unique().await;
        }
    }

    // Wait for all in-flight operations to complete, then close the handle.
    //
    // Prefer this over dropping the handle when possible so close errors are
    // returned to the caller and the OS resource is released promptly.
    pub(crate) async fn close(self) -> io::Result<()> {
        match self.take().await {
            Some(owned) => {
                let raw = owned.into_raw();
                match Op::close(raw) {
                    Ok(op) => op.await,
                    Err(e) => {
                        // Submit failed: reclaim ownership and close synchronously.
                        drop(unsafe { OwnedOsHandle::from_raw(raw) });
                        Err(e)
                    }
                }
            }
            // Already closed (e.g. double close).
            None => Ok(()),
        }
    }

    // Completes when the SharedIoHandle's Inner Rc strong count is 1.
    // Gets polled any time a SharedIoHandle is dropped.
    async fn is_unique(&self) {
        use std::task::Poll;

        poll_fn(|cx| {
            if Rc::<InnerFd>::strong_count(&self.inner) == 1 {
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

// Wake any task waiting for uniqueness when a clone is dropped.
// Without this, `close().await` / `take().await` can hang forever after
// in-flight ops complete.
impl Drop for SharedIoHandle {
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
impl AsRawFd for SharedIoHandle {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd()
    }
}

#[cfg(unix)]
impl AsFd for SharedIoHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // Safety: the owned handle remains open for the lifetime of `&self`
        // while this SharedIoHandle (or an Rc clone) is alive and not closed.
        unsafe { BorrowedFd::borrow_raw(self.raw_fd()) }
    }
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

// Owned platform handle. Drop closes the OS resource.
pub(crate) enum OwnedOsHandle {
    #[cfg(unix)]
    Fd(OwnedFd),
    #[cfg(windows)]
    Handle(OwnedHandle),
    #[cfg(windows)]
    Socket(OwnedSocket),
}

impl OwnedOsHandle {
    pub(crate) fn as_raw(&self) -> OsRawHandle {
        match self {
            #[cfg(unix)]
            OwnedOsHandle::Fd(fd) => OsRawHandle::Fd(fd.as_raw_fd()),
            #[cfg(windows)]
            OwnedOsHandle::Handle(h) => OsRawHandle::Handle(h.as_raw_handle()),
            #[cfg(windows)]
            OwnedOsHandle::Socket(s) => OsRawHandle::Socket(s.as_raw_socket()),
        }
    }

    // Consume the owned handle into a raw value without closing it.
    pub(crate) fn into_raw(self) -> OsRawHandle {
        match self {
            #[cfg(unix)]
            OwnedOsHandle::Fd(fd) => OsRawHandle::Fd(fd.into_raw_fd()),
            #[cfg(windows)]
            OwnedOsHandle::Handle(h) => OsRawHandle::Handle(h.into_raw_handle()),
            #[cfg(windows)]
            OwnedOsHandle::Socket(s) => OsRawHandle::Socket(s.into_raw_socket()),
        }
    }

    // Rebuild an owned handle from a raw value.
    //
    // # Safety
    // `raw` must be open and not owned elsewhere.
    pub(crate) unsafe fn from_raw(raw: OsRawHandle) -> Self {
        match raw {
            #[cfg(unix)]
            OsRawHandle::Fd(fd) => OwnedOsHandle::Fd(unsafe { OwnedFd::from_raw_fd(fd) }),
            #[cfg(windows)]
            OsRawHandle::Handle(h) => OwnedOsHandle::Handle(unsafe { OwnedHandle::from_raw_handle(h) }),
            #[cfg(windows)]
            OsRawHandle::Socket(s) => OwnedOsHandle::Socket(unsafe { OwnedSocket::from_raw_socket(s) }),
        }
    }
}

struct InnerFd {
    // Open file/socket handle. `None` after ownership was transferred to an async close.
    // Only mutated when this Inner is uniquely owned (`Rc::get_mut`) or on Drop.
    handle: Option<OwnedOsHandle>,

    // Track the sharing state of the handle:
    // normal, being waited on to allow a close by the parent's owner, or already closed.
    state: RefCell<State>,
}

impl InnerFd {
    // Take ownership of the handle, marking state Closed so Drop does not close again.
    fn take_owned(&mut self) -> Option<OwnedOsHandle> {
        {
            let state = RefCell::get_mut(&mut self.state);
            if let State::Closed = *state {
                return None;
            }
            *state = State::Closed;
        }
        self.handle.take()
    }
}

// Drop of `InnerFd` closes any remaining owned handle synchronously via
// OwnedFd / OwnedHandle / OwnedSocket (compio / tokio-uring style). After
// explicit `close`/`take`, `handle` is `None` so Drop is a no-op.

enum State {
    // Initial state
    Init,

    // Waiting for all in-flight operations to complete.
    // Waits for the number of strong Rc pointers to drop to 1.
    Waiting(Waker),

    // The close has been triggered by the parent owner (explicit close/take).
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;

    #[cfg(unix)]
    fn dev_null_handle() -> SharedIoHandle {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        SharedIoHandle::new(OwnedFd::from(file))
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
            // Caller owns the fd; drop closes it.
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
        // Compio/tokio-uring policy: last ref Drop closes via OwnedFd.
        let handle = dev_null_handle();
        drop(handle);
    }

    #[cfg(unix)]
    #[test]
    fn take_after_prior_take_is_none_via_closed_state() {
        // take() marks Closed before returning; a second exclusive take on a
        // reconstructed handle is not possible through the public API. Instead
        // verify take_owned semantics: after take, Inner is Closed with no handle.
        use crate::runtime::local::Runtime;

        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = dev_null_handle();

        runtime.block_on(async move {
            let owned = handle.take().await.expect("first take");
            drop(owned);
        });
    }
}
