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
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Truncate {
    handle: SharedIoHandle<std::fs::File>,
    size: u64,
}

impl Op<Truncate> {
    pub(crate) fn truncate(handle: &SharedIoHandle<std::fs::File>, size: u64) -> io::Result<Op<Truncate>> {
        let data = Truncate {
            handle: handle.clone(),
            size,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Truncate {
    type Output = io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::Ftruncate::new(types::Fd(self.handle.raw_fd()), self.size).build()
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Truncate {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let fd = self.handle.raw_fd();
        let size = self.size;
        kqueue_syscall_blocking!({
            // Safety: the operation retains the owning file handle until this
            // blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            rustix::fs::ftruncate(fd, size)
                .map(|()| 0_u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Truncate {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Storage::FileSystem::{SetEndOfFile, SetFilePointerEx};

        let handle = self.handle.raw_handle() as isize;
        let distance_to_move: i64 = self.size as i64;

        windows_syscall_blocking!({
            let mut new_file_pointer: i64 = 0;
            match windows_syscall!(BOOL, {
                SetFilePointerEx(
                    handle as _,
                    distance_to_move,
                    &mut new_file_pointer,
                    windows_sys::Win32::Storage::FileSystem::FILE_BEGIN,
                )
            }) {
                Ok(_) => windows_syscall!(BOOL, SetEndOfFile(handle as _)),
                Err(err) => Err(err),
            }
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
