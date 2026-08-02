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
        helpers::io_handle::OsRawHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Close {
    io_handle: OsRawHandle,
}

impl Op<Close> {
    pub(crate) fn close(handle: OsRawHandle) -> io::Result<Self> {
        let data = Close { io_handle: handle };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Close {
    type Output = io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};
        let fd = match self.io_handle {
            OsRawHandle::Fd(fd) => fd,
        };
        opcode::Close::new(types::Fd(fd)).build()
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        // If the cancel op is successful we don't have to do anything else for it
        let _ = cqe.result?;

        Ok(())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Close {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        // Own the raw fd for the pool job (Close owns the handle exclusively).
        let fd = match self.io_handle {
            OsRawHandle::Fd(fd) => fd,
        };
        kqueue_syscall_blocking!({ kqueue_syscall!(libc::close(fd)) })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        // If the cancel op is successful we don't have to do anything else for it
        let _ = cqe.result?;

        Ok(())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Close {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::{Foundation::CloseHandle, Networking::WinSock::closesocket};

        // Capture as integer types so the blocking job is Send (RawHandle is *mut c_void).
        match self.io_handle {
            OsRawHandle::Handle(h) => {
                let h = h as isize;
                windows_syscall_blocking!({ windows_syscall!(BOOL, CloseHandle(h as _)) })
            }
            OsRawHandle::Socket(s) => {
                windows_syscall_blocking!({ windows_syscall!(SOCKET, closesocket(s as _)) })
            }
        }
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        // If the cancel op is successful we don't have to do anything else for it
        let _ = cqe.result?;

        Ok(())
    }
}
