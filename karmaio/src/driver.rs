use crate::driver::backends::{Operation, PlatformBackend};
use crate::runtime::blocking::BlockingPoolHandle;
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::AsRawFd;
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
pub(crate) mod op_table;
pub(crate) mod ops;

#[cfg(windows)]
use crate::driver::helpers::io_handle::HandleRegistration;
use crate::driver::ops::{DeferredAction, Op};

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
    #[cfg(windows)]
    association: crate::driver::backends::iocp::IocpAssociation,
}

// A weak handle to the driver, plus cloneable tokens that outlive individual
// upgrades (wakeup + blocking pool).
#[derive(Clone)]
pub(crate) struct Handle {
    backend: Weak<RefCell<PlatformBackend>>,
    wakeup: Wakeup,
    blocking: BlockingPoolHandle,
    #[cfg(windows)]
    association: crate::driver::backends::iocp::IocpAssociation,
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
        #[cfg(windows)]
        let association = backend.borrow().association();
        Ok(Self {
            backend,
            wakeup,
            blocking,
            #[cfg(windows)]
            association,
        })
    }

    pub(crate) fn submit_op<T: Operation + 'static>(&self, data: T) -> io::Result<Op<T>> {
        self.backend.borrow_mut().submit_op(data, self.into())
    }

    pub(crate) fn remove_op<T: Operation + 'static>(&self, op: &mut Op<T>) {
        // Run typed complete outside the backend borrow: `complete` may drop
        // user buffers or construct resources that re-enter the driver.
        let completion = self.backend.borrow_mut().remove_op(op);
        if let Some(completion) = completion
            && let Some(data) = op.take_data()
        {
            drop(Operation::complete(data, completion));
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn poll_op<T: Operation + 'static>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output> {
        match self.backend.borrow_mut().poll_op(op, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(completion) => {
                let data = op.take_data().expect("op data missing at completion");
                Poll::Ready(Operation::complete(data, completion))
            }
        }
    }

    #[cfg(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    pub(crate) fn poll_op<T: Operation + 'static>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output> {
        match self.backend.borrow_mut().poll_op(op, cx, &self.blocking, &self.wakeup) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(completion) => {
                let data = op.take_data().expect("op data missing at completion");
                Poll::Ready(Operation::complete(data, completion))
            }
        }
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
        let deferred = self.backend.borrow_mut().drain_blocking_completions();
        DeferredAction::run_all(deferred);
    }

    /// Complete the platform shutdown phase after the blocking pool has
    /// stopped producing work. This keeps backend cleanup independent of Rust
    /// field-drop order and leaves `Drop` as an idempotent backstop.
    pub(crate) fn shutdown(&self) {
        let deferred = self.backend.borrow_mut().shutdown();
        DeferredAction::run_all(deferred);
    }

    /// Apply platform I/O completions (kevent / IOCP / io_uring CQEs).
    pub(crate) fn dispatch_completions(&self) -> io::Result<()> {
        let deferred = self.backend.borrow_mut().dispatch_completions()?;
        DeferredAction::run_all(deferred);
        Ok(())
    }

    /// Returns a cloneable token that can wake the driver from any thread.
    pub(crate) fn wakeup(&self) -> Wakeup {
        self.wakeup.clone()
    }

    /// Returns a handle to the blocking thread pool associated with this driver.
    pub(crate) fn blocking_pool(&self) -> &BlockingPoolHandle {
        &self.blocking
    }

    /// Build a driver around an already-constructed backend for unit tests.
    ///
    /// Uses a no-op cross-thread wakeup: remote `Wakeup::wake` calls do not
    /// unblock `wait`. Prefer [`Driver::new`] for any production path.
    #[cfg(test)]
    pub(crate) fn for_tests(backend: PlatformBackend, blocking: BlockingPoolHandle) -> Self {
        let backend = Rc::new(RefCell::new(backend));
        let wakeup = Wakeup::new(|| {});
        #[cfg(windows)]
        let association = backend.borrow().association();
        Self {
            backend,
            wakeup,
            blocking,
            #[cfg(windows)]
            association,
        }
    }
}

#[cfg(unix)]
impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.backend.borrow().as_raw_fd()
    }
}

impl Handle {
    pub(crate) fn upgrade(&self) -> Option<Driver> {
        let backend = self.backend.upgrade()?;
        Some(Driver {
            backend,
            wakeup: self.wakeup.clone(),
            blocking: self.blocking.clone(),
            #[cfg(windows)]
            association: self.association.clone(),
        })
    }

    /// Associates a resource with this runtime's IOCP without borrowing the
    /// backend. This is also safe to call while the backend is decoding a
    /// completion and constructing an operation-created resource.
    #[cfg(windows)]
    pub(crate) fn associate(&self, registration: &HandleRegistration) -> io::Result<()> {
        self.association.associate(registration)
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
            #[cfg(windows)]
            association: driver.association.clone(),
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
