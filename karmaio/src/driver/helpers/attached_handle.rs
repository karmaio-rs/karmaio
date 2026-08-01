//! Handle associated with the runtime's I/O driver for completion-based I/O.
//!
//! [`AttachedHandle<T>`] wraps a [`SharedIoHandle<T>`] and associates the
//! underlying OS handle with the current runtime's I/O driver on construction.
//! On Windows, this associates the handle with the IOCP completion port. On
//! Linux/macOS, this is a no-op.
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
/// - **Linux (io-uring) / macOS (kqueue):** No-op (returns `Ok(())`).
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
    /// * The user should ensure that the source is associated with the current
    ///   driver.
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
        CURRENT_DRIVER.with(|handle| -> io::Result<()> {
            let driver = handle.upgrade().expect("not in a runtime context");
            driver.attach(source.as_raw_handle())
        })?;
        Ok(unsafe { Self::new_unchecked(source) })
    }
}

#[cfg(unix)]
impl<T: AsFd> AttachedHandle<T> {
    /// Create [`AttachedHandle`]. It tries to associate the source with the
    /// current runtime's driver, and will return [`Err`] if it fails.
    ///
    /// On Linux (io-uring) / macOS (kqueue), this is a no-op.
    pub fn new(source: T) -> io::Result<Self> {
        CURRENT_DRIVER.with(|handle| -> io::Result<()> {
            let driver = handle.upgrade().expect("not in a runtime context");
            driver.attach(source.as_fd().as_raw_fd())
        })?;
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
}

impl<T> Clone for AttachedHandle<T> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
        }
    }
}

// --- From conversions ---

impl<T> From<SharedIoHandle<T>> for AttachedHandle<T> {
    /// Wrap a [`SharedIoHandle`] without associating. Use only when the handle
    /// is already associated with the current driver.
    fn from(source: SharedIoHandle<T>) -> Self {
        Self { source }
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
