use std::io;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::driver::backends::kqueue::{KqueueAttempt, KqueueOperation};

use crate::{
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    fs::Metadata,
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Stat {
    handle: SharedIoHandle<std::fs::File>,
    #[cfg(target_os = "linux")]
    statx_buf: Box<libc::statx>,
}

impl Op<Stat> {
    pub(crate) fn stat(handle: &SharedIoHandle<std::fs::File>) -> io::Result<Op<Stat>> {
        #[cfg(target_os = "linux")]
        let data = Stat {
            handle: handle.clone(),
            statx_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        let data = Stat { handle: handle.clone() };

        #[cfg(windows)]
        let data = Stat { handle: handle.clone() };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Stat {
    type Output = io::Result<Metadata>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let buf_ptr = self.statx_buf.as_mut() as *mut libc::statx as *mut types::statx;

        // Use AT_EMPTY_PATH with an empty path to stat the file descriptor itself.
        // Passing a null path without the flag is invalid for fd-based statx.
        let empty: *const libc::c_char = b"\0".as_ptr() as *const _;

        opcode::Statx::new(types::Fd(self.handle.raw_fd()), empty, buf_ptr)
            .flags(libc::AT_EMPTY_PATH)
            .mask(libc::STATX_BASIC_STATS | libc::STATX_BTIME)
            .build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        Ok(Metadata::from_statx(*self.statx_buf))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Stat {
    type Output = io::Result<Metadata>;
    fn attempt(&mut self) -> KqueueAttempt {
        let fd = self.handle.raw_fd();

        kqueue_syscall_blocking!(value {
            // Safety: the operation retains the owning file handle until this
            // blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            rustix::fs::fstat(fd).map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        let stat = completion.into_blocking_value::<rustix::fs::Stat>()?;
        Ok(Metadata::from_stat(stat))
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Stat {
    type Output = io::Result<Metadata>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Foundation::HANDLE;

        let handle = self.handle.raw_handle() as isize;

        windows_syscall_blocking!(value {
            Metadata::from_handle(handle as HANDLE)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.into_blocking_value::<Metadata>()
    }
}
