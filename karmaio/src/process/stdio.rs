//! Child process standard streams (`stdin`/`stdout`/`stderr`).
//!
//! These wrap the OS pipe ends handed to us by [`std::process::Child`] and
//! expose them through the crate's [`AsyncRead`]/[`AsyncWrite`] traits.
//! The underlying pipe ends are stored as a cloneable [`SharedIoHandle`] so that the
//! completion driver owns a reference for the duration of each in-flight operation;
//! dropping the user-facing handle closes the pipe end exactly once (shared handles via fd/handle duplication)
//! and never races with a submission in progress.
//!
//! [`AsyncRead`]: crate::io::AsyncRead
//! [`AsyncWrite`]: crate::io::AsyncWrite

use std::io;

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    io::{AsyncRead, AsyncWrite},
};

/// A handle to a child process's standard input (writable).
pub struct ChildStdin {
    handle: Option<SharedIoHandle>,
}

/// A handle to a child process's standard output (readable).
pub struct ChildStdout {
    handle: Option<SharedIoHandle>,
}

/// A handle to a child process's standard error (readable).
pub struct ChildStderr {
    handle: Option<SharedIoHandle>,
}

impl From<std::process::ChildStdin> for ChildStdin {
    fn from(io: std::process::ChildStdin) -> Self {
        Self {
            handle: Some(SharedIoHandle::from(io)),
        }
    }
}

impl From<std::process::ChildStdout> for ChildStdout {
    fn from(io: std::process::ChildStdout) -> Self {
        Self {
            handle: Some(SharedIoHandle::from(io)),
        }
    }
}

impl From<std::process::ChildStderr> for ChildStderr {
    fn from(io: std::process::ChildStderr) -> Self {
        Self {
            handle: Some(SharedIoHandle::from(io)),
        }
    }
}

impl From<std::process::ChildStdin> for SharedIoHandle {
    fn from(io: std::process::ChildStdin) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::{FromRawFd, IntoRawFd};
            SharedIoHandle::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(io.into_raw_fd()) })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, IntoRawHandle};
            SharedIoHandle::new_file(unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(io.into_raw_handle() as _)
            })
        }
    }
}

impl From<std::process::ChildStdout> for SharedIoHandle {
    fn from(io: std::process::ChildStdout) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::{FromRawFd, IntoRawFd};
            SharedIoHandle::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(io.into_raw_fd()) })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, IntoRawHandle};
            SharedIoHandle::new_file(unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(io.into_raw_handle() as _)
            })
        }
    }
}

impl From<std::process::ChildStderr> for SharedIoHandle {
    fn from(io: std::process::ChildStderr) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::{FromRawFd, IntoRawFd};
            SharedIoHandle::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(io.into_raw_fd()) })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, IntoRawHandle};
            SharedIoHandle::new_file(unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(io.into_raw_handle() as _)
            })
        }
    }
}

impl ChildStdin {
    /// Consume the handle, returning the underlying OS fd/handle.
    ///
    /// The [`SharedIoHandle`] is deliberately *forgotten* (not dropped) so the
    /// fd/handle is left open; callers that take it are responsible for closing it.
    /// This lets the value cross thread boundaries (it is `Send`) for use on
    /// the blocking pool, where the `Rc`-backed handle cannot go.
    pub(crate) fn into_raw_fd(mut self) -> Option<usize> {
        let handle = self.handle.take()?;
        #[cfg(unix)]
        let raw = handle.raw_fd() as usize;
        #[cfg(windows)]
        let raw = handle.raw_handle() as usize;
        std::mem::forget(handle);
        Some(raw)
    }
}

impl ChildStdout {
    /// Consume the handle, returning the underlying OS fd/handle.
    ///
    /// See [`ChildStdin::into_raw_fd`] for the ownership contract.
    pub(crate) fn into_raw_fd(mut self) -> Option<usize> {
        let handle = self.handle.take()?;
        #[cfg(unix)]
        let raw = handle.raw_fd() as usize;
        #[cfg(windows)]
        let raw = handle.raw_handle() as usize;
        std::mem::forget(handle);
        Some(raw)
    }
}

impl ChildStderr {
    /// Consume the handle, returning the underlying OS fd/handle.
    ///
    /// See [`ChildStdin::into_raw_fd`] for the ownership contract.
    pub(crate) fn into_raw_fd(mut self) -> Option<usize> {
        let handle = self.handle.take()?;
        #[cfg(unix)]
        let raw = handle.raw_fd() as usize;
        #[cfg(windows)]
        let raw = handle.raw_handle() as usize;
        std::mem::forget(handle);
        Some(raw)
    }
}

impl AsyncRead for ChildStdout {
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::read(handle, buf).unwrap().await,
            None => (Ok(0), buf),
        }
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        for buf in bufs {
            match self.read(buf).await {
                (Ok(n), buf) => {
                    total += n;
                    returned.push(buf);
                }
                (Err(e), buf) => {
                    returned.push(buf);
                    return (Err(e), returned);
                }
            }
        }
        (Ok(total), returned)
    }
}

impl AsyncRead for ChildStderr {
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::read(handle, buf).unwrap().await,
            None => (Ok(0), buf),
        }
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        for buf in bufs {
            match self.read(buf).await {
                (Ok(n), buf) => {
                    total += n;
                    returned.push(buf);
                }
                (Err(e), buf) => {
                    returned.push(buf);
                    return (Err(e), returned);
                }
            }
        }
        (Ok(total), returned)
    }
}

impl AsyncWrite for ChildStdin {
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::write(handle, buf).unwrap().await,
            None => (Ok(0), buf),
        }
    }

    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        for buf in bufs {
            match self.write(buf).await {
                (Ok(n), buf) => {
                    total += n;
                    returned.push(buf);
                }
                (Err(e), buf) => {
                    returned.push(buf);
                    return (Err(e), returned);
                }
            }
        }
        (Ok(total), returned)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.handle.take();
        Ok(())
    }
}

impl Drop for ChildStdin {
    fn drop(&mut self) {
        self.handle.take();
    }
}

impl Drop for ChildStdout {
    fn drop(&mut self) {
        self.handle.take();
    }
}

impl Drop for ChildStderr {
    fn drop(&mut self) {
        self.handle.take();
    }
}
