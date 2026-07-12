//! Asynchronous process reaping for macOS via kqueue `EVFILT_PROC` with
//! `NOTE_EXIT`. This is the same mechanism tokio uses for child processes on
//! macOS/BSD: instead of blocking in a thread, we register an interest on the
//! child's pid and only leave the completion driver once the kernel reports the
//! process has exited, at which point we reap it with `Child::wait`.
//!
//! On other platforms `Child::wait` still runs on the blocking pool (see
//! [`crate::process::child`]); a pidfd (`io_uring` `PollAdd`) / IOCP
//! implementation is a future enhancement.

use std::{io, os::fd::RawFd, process};

use crate::{
    driver::{
        backends::kqueue::Interest,
        ops::{Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct WaitProcess {
    child: process::Child,
    pid: u32,
}

impl Op<WaitProcess> {
    pub(crate) fn wait_process(child: process::Child) -> io::Result<Op<WaitProcess>> {
        let pid = child.id();
        let data = WaitProcess { child, pid };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in async runtime").submit_op(data))
    }
}

impl Operable for WaitProcess {}

impl Submittable for WaitProcess {
    fn submit(&mut self) -> crate::driver::ops::Submission {
        let mut interest = Interest::new(self.pid as RawFd, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ONESHOT);
        interest.as_kevent_mut().fflags = libc::NOTE_EXIT;
        crate::driver::ops::Submission::Register(interest)
    }
}

impl crate::driver::ops::Completable for WaitProcess {
    type Result = io::Result<process::ExitStatus>;

    fn complete(mut self, _completion: Completion) -> Self::Result {
        self.child.wait()
    }
}
