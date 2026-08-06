//! Handle associated with the runtime's I/O driver for completion-based I/O.
//!
//! [`AttachedHandle<T>`] wraps a [`SharedIoHandle<T>`] and associates the
//! underlying OS handle with the current runtime's I/O driver on construction.
//! On Windows, this associates the handle with the IOCP completion port. On
//! Linux and macOS/BSD kqueue targets, this is a no-op.
//!
//! # Usage
//!
//! ```rust,ignore
//! use karmaio::driver::helpers::attached_handle::AttachedHandle;
//!
//! let file = std::fs::File::open("foo.txt")?;
//! let handle = AttachedHandle::new(file)?;  // Associates with IOCP on Windows
//! ```

use std::{io, ops::Deref};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, RawHandle, RawSocket};

use super::io_handle::SharedIoHandle;
#[cfg(windows)]
use crate::runtime::local::CURRENT_DRIVER;

/// A handle associated with the runtime's I/O driver.
///
/// A handle can only be associated once with one driver. The associated handle
/// will try to associate the handle on construction and return an error if it
/// fails.
///
/// # Platform-specific behavior
/// - **Windows (IOCP):** Calls `CreateIoCompletionPort` to associate the handle
///   with the completion port and disables per-handle event objects.
/// - **Linux (io-uring) / macOS and BSDs (kqueue):** No-op (returns `Ok(())`).
pub(crate) struct AttachedHandle<T> {
    source: SharedIoHandle<T>,
}

impl<T> std::fmt::Debug for AttachedHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachedHandle").finish_non_exhaustive()
    }
}

impl<T> AttachedHandle<T> {
    /// Create [`AttachedHandle`] without trying to associate the source.
    ///
    /// # Safety
    ///
    /// * The source must already be associated with the current runtime before
    ///   it is used for IOCP operations.
    /// * `T` should be an owned fd/handle.
    pub unsafe fn new_unchecked(source: T) -> Self {
        Self {
            source: SharedIoHandle::new(source),
        }
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> AttachedHandle<T> {
    /// Create [`AttachedHandle`]. It tries to associate the source with the
    /// current runtime's driver, and will return [`Err`] if it fails.
    ///
    /// On Windows, this associates the handle with the IOCP completion port.
    pub fn new(source: T) -> io::Result<Self> {
        let source = SharedIoHandle::new(source);
        current_driver()?.associate(&source.handle_registration())?;
        Ok(Self { source })
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> AttachedHandle<T> {
    /// Create a socket [`AttachedHandle`] and associate it with the current
    /// runtime's IOCP port exactly once.
    pub fn new_socket(source: T) -> io::Result<Self> {
        let source = SharedIoHandle::new(source);
        current_driver()?.associate(&source.socket_registration())?;
        Ok(Self { source })
    }
}

#[cfg(windows)]
fn current_driver() -> io::Result<crate::driver::Handle> {
    if !CURRENT_DRIVER.is_set() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "handle attachment requires a running karmaio runtime",
        ));
    }

    CURRENT_DRIVER.with(|handle| {
        if handle.upgrade().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "karmaio runtime is no longer available for handle attachment",
            ));
        }

        Ok(handle.clone())
    })
}

#[cfg(unix)]
impl<T: AsFd> AttachedHandle<T> {
    /// Create [`AttachedHandle`]. Unix backends do not require registration.
    pub fn new(source: T) -> io::Result<Self> {
        Ok(unsafe { Self::new_unchecked(source) })
    }
}

impl<T> Deref for AttachedHandle<T> {
    type Target = SharedIoHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

impl<T> AttachedHandle<T> {
    /// Consume the associated handle and return the inner [`SharedIoHandle`].
    ///
    /// This is useful when you need to call methods that take `self` by value,
    /// such as `close()`.
    pub(crate) fn into_inner(self) -> SharedIoHandle<T> {
        self.source
    }

    /// Tries to consume this handle and return its resource without closing it.
    pub(crate) fn try_unwrap(self) -> Result<T, Self> {
        self.source.try_unwrap().map_err(|source| Self { source })
    }
}

impl<T> Clone for AttachedHandle<T> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
        }
    }
}

// --- Platform-specific trait impls ---

#[cfg(unix)]
impl<T: AsFd> AsFd for AttachedHandle<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.source.as_fd()
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for AttachedHandle<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.source.raw_fd()
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> AsRawHandle for AttachedHandle<T> {
    fn as_raw_handle(&self) -> RawHandle {
        self.source.raw_handle()
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> AsRawSocket for AttachedHandle<T> {
    fn as_raw_socket(&self) -> RawSocket {
        self.source.raw_socket()
    }
}

// --- FromRaw conversions ---

#[cfg(windows)]
impl<T: FromRawHandle> FromRawHandle for AttachedHandle<T> {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        unsafe { Self::new_unchecked(T::from_raw_handle(handle)) }
    }
}

#[cfg(windows)]
impl<T: FromRawSocket> FromRawSocket for AttachedHandle<T> {
    unsafe fn from_raw_socket(sock: RawSocket) -> Self {
        unsafe { Self::new_unchecked(T::from_raw_socket(sock)) }
    }
}

#[cfg(unix)]
impl<T: FromRawFd> FromRawFd for AttachedHandle<T> {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        unsafe { Self::new_unchecked(T::from_raw_fd(fd)) }
    }
}
