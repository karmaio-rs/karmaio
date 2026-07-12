//! A spawned child process, mirroring [`std::process::Child`].
//!
//! The child's pipes (when configured with [`std::process::Stdio::piped`]) are
//! exposed as [`ChildStdin`], [`ChildStdout`] and [`ChildStderr`], which
//! implement [`AsyncRead`]/[`AsyncWrite`] and are driven by the completion
//! driver. Waiting is asynchronous: the blocking `wait` is offloaded to the
//! runtime's blocking pool so the executor is never blocked on the child.

use std::{
    io,
    process::{Child as StdChild, Command as StdCommand, ExitStatus, Output},
};

use crate::{
    io::AsyncRead,
    process::stdio::{ChildStderr, ChildStdin, ChildStdout},
    runtime::local::{spawn_blocking, spawn_local},
};

#[cfg(target_os = "linux")]
use crate::driver::ops::Op;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};

/// A handle to a spawned child process.
///
/// The handle can be awaited (via [`Child::wait`]) to completion, polled
/// without blocking ([`Child::try_wait`]), signalled ([`Child::kill`]), or have
/// its piped standard streams taken out ([`Child::take_stdin`] and friends).
pub struct Child {
    /// The underlying standard-library child, taken (`None`) once it has been
    /// moved into a `wait` future.
    child: Option<StdChild>,
    /// Cached exit status, set once the child has been reaped. Lets `wait`
    /// be idempotent and `try_wait` report completion even after the inner
    /// `std::process::Child` has been consumed by an in-flight async wait.
    status: Option<ExitStatus>,
    /// The child's standard input, present only when piped.
    pub(crate) stdin: Option<ChildStdin>,
    /// The child's standard output, present only when piped.
    pub(crate) stdout: Option<ChildStdout>,
    /// The child's standard error, present only when piped.
    pub(crate) stderr: Option<ChildStderr>,
    kill_on_drop: bool,
}

impl Child {
    /// Spawns a child from a configured standard-library `Command`.
    pub(crate) fn spawn(inner: &mut StdCommand, kill_on_drop: bool) -> io::Result<Child> {
        let child = inner.spawn()?;
        Ok(Child::from_std(child, kill_on_drop))
    }

    /// Wraps an already-spawned [`std::process::Child`],
    /// taking ownership of its piped stdio handles.
    pub(crate) fn from_std(mut child: StdChild, kill_on_drop: bool) -> Child {
        let stdin = child.stdin.take().map(ChildStdin::from);
        let stdout = child.stdout.take().map(ChildStdout::from);
        let stderr = child.stderr.take().map(ChildStderr::from);
        Child {
            child: Some(child),
            status: None,
            stdin,
            stdout,
            stderr,
            kill_on_drop,
        }
    }

    /// Returns the OS-assigned process identifier, if the child is still alive.
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Attempts to reap the child without blocking. Returns `Ok(Some(status))`
    /// if it has exited, `Ok(None)` if it is still running.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status.clone() {
            return Ok(Some(status));
        }
        match self.child.as_mut() {
            Some(child) => {
                let status = child.try_wait()?;
                if let Some(status) = status {
                    self.status = Some(status);
                }
                Ok(status)
            }
            None => Ok(self.status.clone()),
        }
    }

    /// Waits for the child to exit completely, returning its exit status.
    ///
    /// Reaping is performed asynchronously: the blocking `wait` runs on the
    /// runtime's blocking pool, so the executor is never blocked on the child.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status.clone() {
            return Ok(status);
        }
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let mut child = self.child.take().expect("wait already in progress");

        // On Linux, prefer an async pidfd wait; fall back to
        // the blocking pool if a pidfd cannot be obtained.
        #[cfg(target_os = "linux")]
        if let Ok(pidfd) = pidfd_for_child(child.id()) {
            let status = Op::wait_process(child, pidfd)?.await?;
            self.status = Some(status);
            return Ok(status);
        }

        let status = spawn_blocking(move || child.wait())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "failed to wait for child"))??;
        self.status = Some(status);
        Ok(status)
    }

    /// Forces the child to exit.
    ///
    /// Returns an error if the child has already been `.wait()`-ed on.
    pub fn kill(&mut self) -> io::Result<()> {
        match self.child.as_mut() {
            Some(child) => child.kill(),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has already been awaited",
            )),
        }
    }

    /// Initiates a kill without waiting for the child to finish,
    /// mirroring [`std::process::Child::start_kill`].
    pub fn start_kill(&mut self) -> io::Result<()> {
        match self.child.as_mut() {
            Some(child) => child.kill(),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has already been awaited",
            )),
        }
    }

    /// Takes the child's standard input handle, if it was piped.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Takes the child's standard output handle, if it was piped.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Takes the child's standard error handle, if it was piped.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// Simultaneously waits for the child to exit and collects all of its output.
    /// The child's `stdin` (if any) is closed first so the child
    /// observes EOF, then both `stdout` and `stderr` are drained concurrently
    /// (avoiding the classic pipe-full deadlock) before reaping.
    ///
    /// The streams are drained through the completion driver on the local runtime
    /// (via [`crate::runtime::local::spawn_local`]) rather than on the
    /// blocking pool: the stream handles are `Rc`-backed and therefore not `Send`.
    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        // Drop stdin so the child sees EOF and flushes any buffered output.
        self.stdin.take();

        // Take the pipe handles and drain them concurrently on the local runtime.
        // They are `Rc`-backed (non-`Send`), so they travel via
        // `spawn_local` tasks rather than the blocking pool.
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();

        let out_handle = spawn_local(async move {
            match stdout {
                Some(mut s) => read_to_end(&mut s).await,
                None => Ok(Vec::new()),
            }
        });
        let err_handle = spawn_local(async move {
            match stderr {
                Some(mut s) => read_to_end(&mut s).await,
                None => Ok(Vec::new()),
            }
        });

        let stdout_data = out_handle
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "failed to read child stdout"))??;
        let stderr_data = err_handle
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "failed to read child stderr"))??;

        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

/// Drains an async reader to end-of-file, accumulating the bytes.
///
/// A missing stream (not piped) yields empty output. Used by
/// [`Child::wait_with_output`] to drain the child's stdout/stderr concurrently
/// on the local runtime.
async fn read_to_end<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        // A fresh buffer each iteration: the read op advances the buffer's init
        // cursor, so reusing it would skip already-read bytes.
        let (res, buf) = reader.read([0u8; 8192]).await;
        let n = res?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Ok(data)
}

/// Opens a pidfd for a live child via the `pidfd_open(2)` syscall.
///
/// Used on Linux to drive an async wait through the completion driver. Returns
/// an error on kernels older than 5.3 (or if the pid is no longer waitable), in
/// which case the caller falls back to the blocking pool.
#[cfg(target_os = "linux")]
fn pidfd_for_child(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: `pidfd_open` is a Linux syscall; `pid` is a live child we own and
    // have not yet reaped, so the pidfd it returns is valid.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened pidfd owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
