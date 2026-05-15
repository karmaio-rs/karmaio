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
    Handle,
    ops::{Op, Operable, Submittable},
};

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
    fn remove_op<T>(&mut self, op: &mut Op<T>);

    // Checks if an operation is still pending/valid.
    fn poll_op<T: Operable>(&mut self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Result>;

    // Pushes any pending operations in the submission queue to the kernel.
    fn submit(&mut self) -> Result<()>;

    // Wait infinitely and process returned events.
    fn wait(&mut self) -> Result<usize>;

    // Wait for specified timeout and process returned events.
    fn wait_with_duration(&mut self, duration: Duration) -> Result<usize>;

    // Checks the completion queue for finished operations and dispatches them.
    fn dispatch_completions(&mut self);
}
