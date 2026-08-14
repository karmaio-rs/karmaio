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

/// Platform driver construction parameters (from [`crate::RuntimeConfig`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DriverConfig {
    pub capacity: usize,
    /// Linux io_uring provided buffer pool size (rounded up to a power of two).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub buffer_pool_size: u16,
    /// Linux io_uring provided buffer length in bytes.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub buffer_pool_buffer_len: usize,
    /// Per-stream pending connection limit for Linux multishot accept.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub multishot_accept_capacity: usize,
}

#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::UringMultishotOperation;
#[cfg(windows)]
use crate::driver::helpers::io_handle::HandleRegistration;
#[cfg(target_os = "linux")]
use crate::driver::ops::Completion;
#[cfg(target_os = "linux")]
use crate::driver::ops::MultiOp;
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
    pub(crate) fn new(blocking: BlockingPoolHandle, config: DriverConfig) -> io::Result<Self> {
        let backend = Rc::new(RefCell::new(PlatformBackend::new(config)?));
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

    /// Return a handle to this runtime's provided buffer pool (Linux only).
    ///
    /// The pool is created lazily on first use. See [`crate::buf::BufferPool`]
    /// and [`crate::buf::PooledBuf`] for ownership and starvation notes.
    #[cfg(target_os = "linux")]
    pub(crate) fn buffer_pool(&self) -> io::Result<crate::buf::BufferPool> {
        self.backend.borrow_mut().buffer_pool()
    }

    /// Return the per-stream pending connection limit for multishot accept.
    #[cfg(target_os = "linux")]
    pub(crate) fn multishot_accept_capacity(&self) -> usize {
        self.backend.borrow().multishot_accept_capacity()
    }

    pub(crate) fn submit_op<T: Operation + 'static>(&self, data: T) -> io::Result<Op<T>> {
        self.backend.borrow_mut().submit_op(data, self.into())
    }

    /// Submit a multishot operation and return a stream of completions.
    ///
    /// Requires Linux 6.12+. See multishot method docs on public types.
    #[cfg(target_os = "linux")]
    pub(crate) fn submit_multi_op<T: UringMultishotOperation + 'static>(&self, data: T) -> io::Result<MultiOp<T>> {
        let (key, data) = self.backend.borrow_mut().submit_multi_op(data)?;
        Ok(MultiOp::new(key, data, self.into()))
    }

    pub(crate) fn remove_op<T: Operation + 'static>(&self, op: &mut Op<T>) {
        // Run typed complete outside the backend borrow: `complete` may drop
        // user buffers or construct resources that re-enter the driver.
        let completion = self.backend.borrow_mut().remove_op(op);
        if let Some(completion) = completion
            && let Some(data) = op.take_data()
        {
            drop(Operation::complete(*data, completion));
        }
    }

    /// Cancel a multishot operation (called from [`MultiOp`] drop).
    #[cfg(target_os = "linux")]
    pub(crate) fn remove_multi_op<T: UringMultishotOperation + 'static>(&self, op: &mut MultiOp<T>) {
        let key = op.key();
        let Some(data) = op.take_data() else {
            return;
        };
        self.backend.borrow_mut().remove_multi_op(key, data);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn poll_op<T: Operation + 'static>(&self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Output> {
        match self.backend.borrow_mut().poll_op(op, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(completion) => {
                let data = op.take_data().expect("op data missing at completion");
                Poll::Ready(Operation::complete(*data, completion))
            }
        }
    }

    /// Poll the next multishot completion (raw CQE), if any.
    #[cfg(target_os = "linux")]
    pub(crate) fn poll_multi_op<T: UringMultishotOperation + 'static>(
        &self,
        op: &mut MultiOp<T>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Completion>> {
        self.backend.borrow_mut().poll_multi_op(op.key(), cx)
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
                Poll::Ready(Operation::complete(*data, completion))
            }
        }
    }

    pub(crate) fn wait(&self) -> io::Result<usize> {
        self.backend.borrow_mut().wait()
    }

    pub(crate) fn wait_with_duration(&self, duration: std::time::Duration) -> io::Result<usize> {
        self.backend.borrow_mut().wait_with_duration(duration)
    }

    /// Perform one platform-driver turn and apply all resulting completions.
    ///
    /// `None` waits until at least one platform event arrives. A duration of
    /// zero performs a nonblocking turn, which lets the scheduler service I/O
    /// without yielding the core while runnable tasks remain.
    pub(crate) fn turn(&self, timeout: Option<std::time::Duration>) -> io::Result<usize> {
        let completed = match timeout {
            Some(duration) => self.wait_with_duration(duration)?,
            None => self.wait()?,
        };
        self.drain_blocking_completions();
        self.dispatch_completions()?;
        Ok(completed)
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

impl std::task::Wake for Wakeup {
    fn wake(self: Arc<Self>) {
        (self.inner)();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        (self.inner)();
    }
}
