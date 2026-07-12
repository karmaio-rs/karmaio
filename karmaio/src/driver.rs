use crate::driver::backends::{DriverBackend, PlatformBackend};
use crate::driver::ops::{Op, Operable, Submittable};
use crate::runtime::blocking::BlockingPoolHandle;
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::AsRawFd;
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

// We expose clean type aliases here so the rest of the runtime (ops, executor, etc.) can use `Driver::Submission
pub(crate) use backends::Submission;

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
    pub(crate) fn new(blocking: BlockingPoolHandle) -> io::Result<Self> {
        let backend = Rc::new(RefCell::new(PlatformBackend::new()?));
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

    pub(crate) fn submit_op<T: Submittable>(&self, data: T) -> io::Result<Op<T>> {
        self.backend.borrow_mut().submit_op(data, self.into())
    }

    pub(crate) fn remove_op<T: 'static>(&self, op: &mut Op<T>) {
        self.backend.borrow_mut().remove_op(op)
    }

    pub(crate) fn poll_op<T: Operable>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Result> {
        self.backend.borrow_mut().poll_op(op, cx)
    }

    pub(crate) fn submit(&self) -> io::Result<()> {
        self.backend.borrow_mut().submit()
    }

    pub(crate) fn wait(&self) -> io::Result<usize> {
        self.backend.borrow_mut().wait()
    }

    pub(crate) fn wait_with_duration(&self, duration: std::time::Duration) -> io::Result<usize> {
        self.backend.borrow_mut().wait_with_duration(duration)
    }

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

    /// Associates a file or socket handle with the IOCP completion port.
    ///
    /// This must be called before issuing any overlapped I/O on the handle.
    #[cfg(windows)]
    pub(crate) fn attach(&self, handle: RawHandle) -> io::Result<()> {
        self.backend.borrow().add_handle(handle, 0)
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
