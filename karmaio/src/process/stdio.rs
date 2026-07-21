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

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    io::{AsyncRead, AsyncWrite},
};

/// Platform pipe-end type stored in child stdio handles.
#[cfg(unix)]
type PipeHandle = SharedIoHandle<OwnedFd>;
#[cfg(windows)]
type PipeHandle = SharedIoHandle<OwnedHandle>;

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
fn take_pipe_fd(io: impl IntoRawFd) -> PipeHandle {
    // Safety: ChildStd* into_raw_fd transfers exclusive ownership of the pipe end.
    SharedIoHandle::new(unsafe { OwnedFd::from_raw_fd(io.into_raw_fd()) })
}

#[cfg(windows)]
fn take_pipe_handle(io: impl IntoRawHandle) -> PipeHandle {
    // Safety: ChildStd* into_raw_handle transfers exclusive ownership of the pipe end.
    SharedIoHandle::new(unsafe { OwnedHandle::from_raw_handle(io.into_raw_handle() as _) })
}

impl From<std::process::ChildStdin> for ChildStdin {
    fn from(io: std::process::ChildStdin) -> Self {
        #[cfg(unix)]
        {
            Self {
                handle: Some(take_pipe_fd(io)),
            }
        }
        #[cfg(windows)]
        {
            Self {
                handle: Some(take_pipe_handle(io)),
            }
        }
    }
}

impl From<std::process::ChildStdout> for ChildStdout {
    fn from(io: std::process::ChildStdout) -> Self {
        #[cfg(unix)]
        {
            Self {
                handle: Some(take_pipe_fd(io)),
            }
        }
        #[cfg(windows)]
        {
            Self {
                handle: Some(take_pipe_handle(io)),
            }
        }
    }
}

impl From<std::process::ChildStderr> for ChildStderr {
    fn from(io: std::process::ChildStderr) -> Self {
        #[cfg(unix)]
        {
            Self {
                handle: Some(take_pipe_fd(io)),
            }
        }
        #[cfg(windows)]
        {
            Self {
                handle: Some(take_pipe_handle(io)),
            }
        }
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
