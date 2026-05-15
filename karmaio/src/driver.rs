use crate::driver::backends::{DriverBackend, PlatformBackend};
use crate::driver::ops::{Op, Operable, Submittable};
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::task::Poll;
use std::{
    cell::RefCell,
    io,
    rc::{Rc, Weak},
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
}

// A weak handle to the driver
#[derive(Clone)]
pub(crate) struct Handle {
    backend: Weak<RefCell<PlatformBackend>>,
}

impl Driver {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            backend: Rc::new(RefCell::new(PlatformBackend::new()?)),
        })
    }

    pub(crate) fn submit_op<T: Submittable>(&self, data: T) -> io::Result<Op<T>> {
        self.backend.borrow_mut().submit_op(data, self.into())
    }

    pub(crate) fn remove_op<T>(&self, op: &mut Op<T>) {
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
}

#[cfg(unix)]
impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.backend.borrow().as_raw_fd()
    }
}

impl From<PlatformBackend> for Driver {
    fn from(driver: PlatformBackend) -> Self {
        Self {
            backend: Rc::new(RefCell::new(driver)),
        }
    }
}

impl Handle {
    pub(crate) fn upgrade(&self) -> Option<Driver> {
        Some(Driver {
            backend: self.backend.upgrade()?,
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
        }
    }
}
