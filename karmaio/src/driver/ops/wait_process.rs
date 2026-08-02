//! Asynchronous process reaping for Linux via a pidfd + io_uring `PollAdd`.
//!
//! This opens a pidfd for the child with `pidfd_open(2)` and poll it for readability;
//! when the kernel reports the process has exited the pidfd becomes readable,
//! we reap it with `Child::wait`.
//! Falls back to the blocking pool (see [`crate::process::child`])
//! when a pidfd cannot be obtained (kernels older than 5.3).

use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
    process,
};

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
use crate::driver::ops::{Completion, Op};
use crate::runtime::local::CURRENT_DRIVER;

pub(crate) struct WaitProcess {
    // Held so the pidfd stays open until the process has been reaped.
    pidfd: OwnedFd,
    // The underlying standard-library child, reaped in `complete`.
    child: process::Child,
}

impl Op<WaitProcess> {
    pub(crate) fn wait_process(child: process::Child, pidfd: OwnedFd) -> io::Result<Op<WaitProcess>> {
        let data = WaitProcess { pidfd, child };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for WaitProcess {
    type Output = io::Result<process::ExitStatus>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};
        // Poll the pidfd for readability; it becomes readable once the child exits.
        opcode::PollAdd::new(types::Fd(self.pidfd.as_raw_fd()), libc::POLLIN as u32).build()
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        reap_after_pidfd_ready(completion, || {
            // The pidfd fired, so the child has exited; reap it.
            // `self.pidfd` is closed when `WaitProcess` is dropped.
            self.child.wait()
        })
    }
}

fn reap_after_pidfd_ready<T>(completion: Completion, reap: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    // A readable pidfd only tells us that the process exited. The CQE itself
    // can still carry a kernel error (for example after a cancellation race),
    // and that error must not be hidden by `wait`.
    completion.result?;
    reap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn failed_pidfd_poll_does_not_wait_for_child() {
        let waited = Cell::new(false);
        let error = reap_after_pidfd_ready(
            Completion {
                result: Err(io::Error::from_raw_os_error(libc::ECANCELED)),
            },
            || {
                waited.set(true);
                Ok(())
            },
        )
        .expect_err("cancelled pidfd poll must fail");

        assert_eq!(error.raw_os_error(), Some(libc::ECANCELED));
        assert!(!waited.get(), "child wait must not run after a failed pidfd poll");
    }
}
