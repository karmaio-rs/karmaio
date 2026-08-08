//! Child process standard streams (`stdin`/`stdout`/`stderr`).
//!
//! These wrap the OS pipe ends handed to us by [`std::process::Child`] and
//! expose them through the crate's [`AsyncRead`]/[`AsyncWrite`] traits.
//! The underlying pipe ends are stored as a cloneable [`SharedIoHandle`] so that the
//! completion driver owns a reference for the duration of each in-flight operation;
//! dropping the user-facing handle closes the pipe end exactly once
//! and never races with a submission in progress.
//!
//! [`AsyncRead`]: crate::io::AsyncRead
//! [`AsyncWrite`]: crate::io::AsyncWrite

use std::io;

#[cfg(unix)]
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};

#[cfg(unix)]
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

use crate::{
    buf::{BufResult, IoBuf, IoBufMut},
    driver::{helpers::attached_handle::AttachedHandle, ops::Op},
    io::{AsyncRead, AsyncWrite},
};

/// Platform pipe-end type stored in child stdio handles.
#[cfg(unix)]
type PipeHandle = AttachedHandle<OwnedFd>;
#[cfg(windows)]
type PipeHandle = AttachedHandle<OwnedHandle>;

/// A handle to a child process's standard input (writable).
pub struct ChildStdin {
    handle: Option<PipeHandle>,
}

/// A handle to a child process's standard output (readable).
pub struct ChildStdout {
    handle: Option<PipeHandle>,
}

/// A handle to a child process's standard error (readable).
pub struct ChildStderr {
    handle: Option<PipeHandle>,
}

#[cfg(unix)]
fn take_pipe_fd(io: impl IntoRawFd) -> io::Result<PipeHandle> {
    // Safety: ChildStd* into_raw_fd transfers exclusive ownership of the pipe end.
    let fd = unsafe { OwnedFd::from_raw_fd(io.into_raw_fd()) };
    // Set non-blocking mode for async I/O readiness notifications (kqueue, epoll, etc.)
    // This is required for readiness-based backends to work correctly.
    if let Ok(flags) = fcntl_getfl(&fd) {
        let _ = fcntl_setfl(&fd, flags | OFlags::NONBLOCK);
    }
    AttachedHandle::new(fd)
}

#[cfg(windows)]
fn take_pipe_handle(io: impl IntoRawHandle) -> io::Result<PipeHandle> {
    // Safety: ChildStd* into_raw_handle transfers exclusive ownership of the pipe end.
    AttachedHandle::new(unsafe { OwnedHandle::from_raw_handle(io.into_raw_handle() as _) })
}

impl ChildStdin {
    pub(crate) fn from_std(io: std::process::ChildStdin) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                handle: Some(take_pipe_fd(io)?),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: Some(take_pipe_handle(io)?),
            })
        }
    }
}

impl ChildStdout {
    pub(crate) fn from_std(io: std::process::ChildStdout) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                handle: Some(take_pipe_fd(io)?),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: Some(take_pipe_handle(io)?),
            })
        }
    }
}

impl ChildStderr {
    pub(crate) fn from_std(io: std::process::ChildStderr) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                handle: Some(take_pipe_fd(io)?),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: Some(take_pipe_handle(io)?),
            })
        }
    }
}

impl AsyncRead for ChildStdout {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::read(handle, buf).unwrap().await,
            None => BufResult(Ok(0), buf),
        }
    }
}

impl AsyncRead for ChildStderr {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::read(handle, buf).unwrap().await,
            None => BufResult(Ok(0), buf),
        }
    }
}

impl AsyncWrite for ChildStdin {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self.handle.as_ref() {
            Some(handle) => Op::write(handle, buf).unwrap().await,
            None => BufResult(Ok(0), buf),
        }
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
