use std::{
    io::Result,
    task::{Context, Poll},
    time::Duration,
};

#[cfg(target_os = "windows")]
pub(crate) mod iocp;
#[cfg(target_os = "linux")]
pub(crate) mod iouring;
#[cfg(target_os = "macos")]
pub(crate) mod kqueue;

use crate::driver::{
    Handle, Wakeup,
    ops::{Op, Operable, Submittable},
};
use crate::runtime::blocking::BlockingPoolHandle;

#[cfg(target_os = "windows")]
pub(crate) use self::iocp::IocpBackend as PlatformBackend;
#[cfg(target_os = "windows")]
pub(crate) use self::iocp::Submission;
#[cfg(target_os = "linux")]
pub(crate) use self::iouring::IoUringBackend as PlatformBackend;
#[cfg(target_os = "linux")]
pub(crate) use self::iouring::Submission;
#[cfg(target_os = "macos")]
pub(crate) use self::kqueue::KqueueBackend as PlatformBackend;
#[cfg(target_os = "macos")]
pub(crate) use self::kqueue::Submission;

pub(crate) trait DriverBackend {
    // Submit a prepared entry to the backend.
    fn submit_op<T: Submittable>(&mut self, data: T, handle: Handle) -> Result<Op<T>>;

    /// Removes an operation from the driver's tracking (version 2).
    fn remove_op<T: 'static>(&mut self, op: &mut Op<T>);

    // Checks if an operation is still pending/valid.
    //
    // `blocking` / `wakeup` are used when an op returns `Submission::Blocking`
    // (macOS / Windows path and fd metadata syscalls). Linux io_uring ignores them.
    fn poll_op<T: Operable>(
        &mut self,
        op: &mut Op<T>,
        cx: &mut Context<'_>,
        blocking: &BlockingPoolHandle,
        wakeup: &Wakeup,
    ) -> Poll<T::Result>;

    /// Flush the submission queue without waiting for completions.
    ///
    /// Called from the runtime cold path when tasks remain after a scheduler batch
    /// (so io_uring SQEs are not held until park). kqueue / IOCP return `Ok(())`.
    fn submit(&mut self) -> Result<()>;

    // Wait infinitely and process returned events.
    fn wait(&mut self) -> Result<usize>;

    // Wait for specified timeout and process returned events.
    fn wait_with_duration(&mut self, duration: Duration) -> Result<usize>;

    /// Apply completions produced by the blocking thread pool.
    ///
    /// The runtime owns the pool and calls this after `wait*` as its own phase,
    /// before platform I/O completion dispatch. Default: no-op (e.g. io_uring).
    fn drain_blocking_completions(&mut self) {}

    // Apply platform I/O completions (CQEs / kevents / IOCP packets).
    // Does not drain the blocking pool — see [`drain_blocking_completions`].
    fn dispatch_completions(&mut self);

    /// Create a `Wakeup` token that can be used from other threads to wake
    /// a currently blocked `wait*` call on this driver.
    fn create_wakeup(&self) -> crate::driver::Wakeup;
}
