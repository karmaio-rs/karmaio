use crate::driver::backends::{Operation, PlatformBackend};
use crate::runtime::blocking::BlockingPoolHandle;
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::task::Poll;
use std::{
    cell::RefCell,
    io,
    rc::{Rc, Weak},
    sync::Arc,
    task::Context,
};

pub(crate) mod backends;
pub(super) mod helpers;
pub(crate) mod ops;

use crate::driver::ops::Op;

// Shared, cloneable handle to the platform driver.
//
// This is the only type the rest of the runtime (futures, ops, executor, waker registration, etc.)
// interacts with. The actual `PlatformBackend` is hidden behind `Rc<RefCell<...>>` so we can
// provide `&self` methods while the backend methods require `&mut self`.
#[derive(Clone)]
pub(crate) struct Driver {
    pub(super) backend: Rc<RefCell<PlatformBackend>>,
    /// Wakeup token for cross thread notifications. Cloned into scheduler handles.
    wakeup: Wakeup,
    /// Handle to the runtime's blocking thread pool for offloading sync work.
    blocking: BlockingPoolHandle,
}

// A weak handle to the driver, plus cloneable tokens that outlive individual
// upgrades (wakeup + blocking pool).
#[derive(Clone)]
pub(crate) struct Handle {
    backend: Weak<RefCell<PlatformBackend>>,
    wakeup: Wakeup,
    blocking: BlockingPoolHandle,
}

impl Driver {
    pub(crate) fn new(blocking: BlockingPoolHandle, capacity: usize) -> io::Result<Self> {
        let backend = Rc::new(RefCell::new(PlatformBackend::new(capacity)?));
        // Create the wakeup token while we have access to the (non-Send) backend.
        // The token itself is Send+Sync+Clone and captures only thread-safe poke data.
        let wakeup = {
            let b = backend.borrow();
            b.create_wakeup()
        };
        Ok(Self {
            backend,
            wakeup,
            blocking,
        })
    }

    pub(crate) fn submit_op<T: Operation + 'static>(&self, data: T) -> io::Result<Op<T>> {
        self.backend.borrow_mut().submit_op(data, self.into())
    }

    pub(crate) fn remove_op<T: Operation + 'static>(&self, op: &mut Op<T>) {
        self.backend.borrow_mut().remove_op(op)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn poll_op<T: Operation + 'static>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output> {
        self.backend.borrow_mut().poll_op(op, cx)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn poll_op<T: Operation + 'static>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output> {
        self.backend.borrow_mut().poll_op(op, cx, &self.blocking, &self.wakeup)
    }

    /// Flush the backend submission queue without waiting for completions.
    ///
    /// On io_uring this submits pending SQEs. On kqueue / IOCP this is a no-op
    /// (those backends submit synchronously in `poll_op` / `submit_op`).
    /// Called from the runtime cold path when tasks remain after a batch.
    pub(crate) fn submit(&self) -> io::Result<()> {
        self.backend.borrow_mut().submit()
    }

    pub(crate) fn wait(&self) -> io::Result<usize> {
        self.backend.borrow_mut().wait()
    }

    pub(crate) fn wait_with_duration(&self, duration: std::time::Duration) -> io::Result<usize> {
        self.backend.borrow_mut().wait_with_duration(duration)
    }

    /// Apply completions from the blocking thread pool.
    ///
    /// Called by the runtime after `wait*` so pool results are merged into op
    /// state (and waiters woken) before the next scheduler tick.
    pub(crate) fn drain_blocking_completions(&self) {
        self.backend.borrow_mut().drain_blocking_completions();
    }

    /// Apply platform I/O completions (kevent / IOCP / io_uring CQEs).
    pub(crate) fn dispatch_completions(&self) {
        self.backend.borrow_mut().dispatch_completions();
    }

    /// Returns a cloneable token that can wake the driver from any thread.
    pub(crate) fn wakeup(&self) -> Wakeup {
        self.wakeup.clone()
    }

    /// Returns a handle to the blocking thread pool associated with this driver.
    pub(crate) fn blocking_pool(&self) -> &BlockingPoolHandle {
        &self.blocking
    }

    /// Associates a file or socket handle with the driver's I/O mechanism.
    ///
    /// On Windows (IOCP), this calls `CreateIoCompletionPort` and sets
    /// `SetFileCompletionNotificationModes` for optimal performance. On
    /// Linux (io-uring) / macOS (kqueue), this is a no-op.
    #[cfg(windows)]
    pub(crate) fn attach(&self, handle: RawHandle) -> io::Result<()> {
        self.backend.borrow().attach(handle)
    }

    /// Associates a file descriptor with the driver's I/O mechanism.
    ///
    /// On Linux (io-uring) / macOS (kqueue), this is a no-op.
    #[cfg(unix)]
    pub(crate) fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.backend.borrow().attach(fd)
    }
}

#[cfg(unix)]
impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.backend.borrow().as_raw_fd()
    }
}

impl From<(PlatformBackend, BlockingPoolHandle)> for Driver {
    fn from((driver, blocking): (PlatformBackend, BlockingPoolHandle)) -> Self {
        let backend = Rc::new(RefCell::new(driver));
        // No real wakeup available in this path; use a no-op. This path is
        // primarily for tests or special construction and cross-thread wake
        // may not be required.
        let wakeup = Wakeup::new(|| {});
        Self {
            backend,
            wakeup,
            blocking,
        }
    }
}

impl Handle {
    pub(crate) fn upgrade(&self) -> Option<Driver> {
        let backend = self.backend.upgrade()?;
        Some(Driver {
            backend,
            wakeup: self.wakeup.clone(),
            blocking: self.blocking.clone(),
        })
    }
}

impl<T> From<T> for Handle
where
    T: Deref<Target = Driver>,
{
    fn from(driver: T) -> Self {
        Self {
            backend: Rc::downgrade(&driver.backend),
            wakeup: driver.wakeup.clone(),
            blocking: driver.blocking.clone(),
        }
    }
}

/// A Send + Sync + Clone handle that can wake a blocked driver wait from any thread.
/// Used by the scheduler's remote queue to promptly wake the runtime when a task
/// is scheduled from another thread.
#[derive(Clone)]
pub(crate) struct Wakeup {
    inner: Arc<dyn Fn() + Send + Sync>,
}

impl Wakeup {
    pub(crate) fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self { inner: Arc::new(f) }
    }

    pub(crate) fn wake(&self) {
        (self.inner)();
    }
}
