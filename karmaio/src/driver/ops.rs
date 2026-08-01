use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use crate::driver::Handle;
use crate::driver::backends::Operation;

// Always available: every `SharedIoHandle<T>` path closes through the driver.
pub(crate) mod close;

// Filesystem ops (`feature = "fs"`).
#[cfg(feature = "fs")]
pub(crate) mod create_dir;
#[cfg(feature = "fs")]
pub(crate) mod hardlink;
#[cfg(feature = "fs")]
pub(crate) mod open;
#[cfg(feature = "fs")]
pub(crate) mod read_at;
#[cfg(feature = "fs")]
pub(crate) mod readv;
#[cfg(feature = "fs")]
pub(crate) mod rename;
#[cfg(feature = "fs")]
pub(crate) mod set_permissions;
#[cfg(feature = "fs")]
pub(crate) mod stat;
#[cfg(feature = "fs")]
pub(crate) mod symlink;
#[cfg(feature = "fs")]
pub(crate) mod sync;
#[cfg(feature = "fs")]
pub(crate) mod truncate;
#[cfg(feature = "fs")]
pub(crate) mod unlink;
#[cfg(feature = "fs")]
pub(crate) mod write_at;
#[cfg(feature = "fs")]
pub(crate) mod writev;

// Network ops (`feature = "net"`).
#[cfg(feature = "net")]
pub(crate) mod accept;
#[cfg(feature = "net")]
pub(crate) mod connect;
#[cfg(feature = "net")]
pub(crate) mod recv;
#[cfg(feature = "net")]
pub(crate) mod recv_from;
#[cfg(feature = "net")]
pub(crate) mod recvmsg;
#[cfg(feature = "net")]
pub(crate) mod send;
#[cfg(feature = "net")]
pub(crate) mod send_to;
#[cfg(feature = "net")]
pub(crate) mod sendmsg;

// Process / pipe stream ops (`feature = "process"`).
// Offset-less read/write are used by child stdio pipes (not seekable).
#[cfg(feature = "process")]
pub(crate) mod read;
#[cfg(all(feature = "process", target_os = "linux"))]
pub(crate) mod wait_process;
#[cfg(feature = "process")]
pub(crate) mod write;

/// Terminal result shared by the backend protocols.
///
/// The result intentionally contains only the operation result. Backend
/// completion metadata stays in the backend that owns it, which keeps the
/// common operation contract independent of io_uring CQE flags and kqueue or
/// IOCP details.
pub(crate) struct Completion {
    pub(crate) result: io::Result<u32>,
}

/// A typed one-shot operation future shared by all backends.
///
/// The future owns the logical operation payload. The selected backend owns
/// only the lifecycle slot and any platform-specific submission state, and
/// drives this future through the target-local `Operation` protocol.
pub(crate) struct Op<T: Operation + 'static> {
    driver: Handle,
    index: usize,
    data: Option<T>,
}

impl<T: Operation + 'static> Op<T> {
    pub(crate) fn new(index: usize, data: T, driver: Handle) -> Self {
        Self {
            driver,
            index,
            data: Some(data),
        }
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    #[allow(dead_code)]
    pub(crate) fn data_ref(&self) -> Option<&T> {
        self.data.as_ref()
    }

    #[allow(dead_code)]
    pub(crate) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }
}

impl<T: Operation + Unpin + 'static> Future for Op<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver
            .upgrade()
            .expect("Not in runtime context")
            .poll_op(self.get_mut(), cx)
    }
}

impl<T: Operation + 'static> Drop for Op<T> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.remove_op(self);
        }
    }
}

/// Send-able unit of work for the blocking thread pool.
///
/// Built inside a readiness backend's submission attempt when an operation
/// must run on the blocking pool.
/// Captures only `Send` state (paths, raw fds, flags) so the runtime thread can
/// keep non-`Send` op data (e.g. `SharedIoHandle`) while the syscall runs off-thread.
/// Used on macOS / Windows; io_uring handles equivalent work in-kernel.
#[allow(dead_code)]
pub(crate) struct BlockingJob {
    work: Box<dyn FnOnce() -> Completion + Send + 'static>,
}

#[allow(dead_code)] // Used on macOS / Windows; unused on pure io_uring Linux builds.
impl BlockingJob {
    pub(crate) fn new(work: impl FnOnce() -> Completion + Send + 'static) -> Self {
        Self { work: Box::new(work) }
    }

    pub(crate) fn run(self) -> Completion {
        (self.work)()
    }
}

// Select the concrete protocol and future type at compile time. These aliases
// are target-local names used by the logical operation modules; the backend
// itself still owns the actual submission result and lifecycle protocol.
#[cfg(target_os = "windows")]
pub(crate) use crate::driver::backends::iocp::{
    IocpComplete as BackendComplete, IocpSubmission as BackendSubmission, IocpSubmit as BackendSubmit,
};
#[cfg(target_os = "linux")]
pub(crate) use crate::driver::backends::iouring::{
    Submission as BackendSubmission, UringComplete as BackendComplete, UringSubmit as BackendSubmit,
};
#[cfg(target_os = "macos")]
pub(crate) use crate::driver::backends::kqueue::{
    PollAttempt as BackendSubmission, PollComplete as BackendComplete, PollSubmit as BackendSubmit,
};
