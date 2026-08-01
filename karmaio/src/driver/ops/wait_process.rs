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

use crate::driver::ops::{BackendSubmission, BackendSubmit, Completion, Op};
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

impl BackendSubmit for WaitProcess {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};
        // Poll the pidfd for readability; it becomes readable once the child exits.
        opcode::PollAdd::new(types::Fd(self.pidfd.as_raw_fd()), libc::POLLIN as u32).build()
    }
}

impl crate::driver::ops::BackendComplete for WaitProcess {
    type Result = io::Result<process::ExitStatus>;

    fn complete(mut self, _completion: Completion) -> Self::Result {
        // The pidfd fired, so the child has exited; reap it.
        // `self.pidfd` is closed when `WaitProcess` is dropped.
        self.child.wait()
    }
}
