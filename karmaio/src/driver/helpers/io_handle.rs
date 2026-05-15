use std::{cell::RefCell, future::poll_fn, io, rc::Rc, task::Waker};

#[cfg(windows)]
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, FromRawSocket, RawHandle, RawSocket};

use crate::driver::ops::Op;

// Tracks in-flight operations on a file or socket handle. Ensures all in-flight
// operations complete before submitting the close.
//
// When closing the handle because it is going out of scope, a synchronous close is
// employed.
//
// The closed state is tracked so close calls after the first are ignored.
// Only the first close call returns the true result of closing the handle.
//
// This type is cross-platform (Unix + Windows) and supports both file and socket
// handles using conditional compilation.
#[derive(Clone)]
pub(crate) struct SharedIoHandle {
    inner: Rc<InnerFd>,
}

impl SharedIoHandle {
    #[cfg(unix)]
    pub(crate) fn new(fd: RawFd) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: OsRawHandle::Fd(fd),
                state: RefCell::new(State::Init),
            }),
        }
    }

    #[cfg(windows)]
    pub(crate) fn new_file(handle: RawHandle) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: OsRawHandle::Handle(handle),
                state: RefCell::new(State::Init),
            }),
        }
    }

    #[cfg(windows)]
    pub(crate) fn new_socket(socket: RawSocket) -> SharedIoHandle {
        SharedIoHandle {
            inner: Rc::new(InnerFd {
                handle: OsRawHandle::Socket(socket),
                state: RefCell::new(State::Init),
            }),
        }
    }

    // Returns the RawFd (Unix-only).
    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        match self.inner.handle {
            OsRawHandle::Fd(fd) => fd,
        }
    }

    // Returns the RawHandle (Windows file handle only).
    #[cfg(windows)]
    pub(crate) fn raw_handle(&self) -> RawHandle {
        match self.inner.handle {
            OsRawHandle::Handle(h) => h,
            OsRawHandle::Socket(_) => unreachable!("SharedFd was created with new_socket; use raw_socket"),
        }
    }

    // Returns the RawSocket (Windows socket handle only).
    #[cfg(windows)]
    pub(crate) fn raw_socket(&self) -> RawSocket {
        match self.inner.handle {
            OsRawHandle::Socket(s) => s,
            OsRawHandle::Handle(_) => unreachable!("SharedFd was created with new_file; use raw_handle"),
        }
    }

    // A handle cannot be closed until all in-flight operations have completed.
    // This prevents bugs where in-flight reads/writes could operate on the incorrect
    // handle (or a reused one).
    pub(crate) async fn close(&mut self) -> io::Result<()> {
        loop {
            // Get a mutable reference to Inner, indicating there are no
            // in-flight operations on the handle.
            if let Some(inner) = Rc::get_mut(&mut self.inner) {
                // Wait for the close operation.
                return inner.close_op().await;
            }

            self.is_unique().await;
        }
    }

    // Completes when the SharedFd's Inner Rc strong count is 1.
    // Gets polled any time a SharedFd is dropped.
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

// Platform-specific raw handle (file descriptor on Unix, file handle or socket on Windows).
// This allows a single SharedFd type to work for both files and sockets across platforms.
#[derive(Copy, Clone)]
pub(crate) enum OsRawHandle {
    #[cfg(unix)]
    Fd(RawFd),
    #[cfg(windows)]
    Handle(RawHandle),
    #[cfg(windows)]
    Socket(RawSocket),
}

struct InnerFd {
    // Open file/socket handle
    handle: OsRawHandle,

    // Track the sharing state of the handle:
    // normal, being waited on to allow a close by the parent's owner, or already closed.
    state: RefCell<State>,
}

impl InnerFd {
    async fn close_op(&mut self) -> io::Result<()> {
        todo!("Implement this later")
    }
}

// Ensures the handle is closed if the async close fails.
impl Drop for InnerFd {
    fn drop(&mut self) {
        // If the inner state isn't `Closed`, the user hasn't called close().await
        // so do it synchronously.

        let state = self.state.borrow_mut();

        if let State::Closed = *state {
            return;
        }

        #[cfg(unix)]
        {
            match self.handle {
                OsRawHandle::Fd(fd) => {
                    let _ = unsafe { std::fs::File::from_raw_fd(fd) };
                }
            }
        }

        #[cfg(windows)]
        {
            match self.handle {
                OsRawHandle::Handle(h) => {
                    let _ = unsafe { std::fs::File::from_raw_handle(h) };
                }
                OsRawHandle::Socket(s) => {
                    // TcpStream::from_raw_socket works for any socket type (the drop path
                    // calls closesocket regardless of the concrete socket kind).
                    let _ = unsafe { TcpStream::from_raw_socket(s) };
                }
            }
        }
    }
}

enum State {
    // Initial state
    Init,

    // Waiting for all in-flight operations to complete.
    // Waits for the number of strong Rc pointers to drop to 1.
    Waiting(Waker),

    // The close has been triggered by the parent owner.
    Closed,
}
