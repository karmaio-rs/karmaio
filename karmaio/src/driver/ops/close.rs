use std::io;

use crate::{
    driver::{
        Submission,
        helpers::io_handle::OsRawHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
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

impl Operable for Close {}

#[cfg(target_os = "linux")]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};
        let fd = match self.io_handle {
            OsRawHandle::Fd(fd) => fd,
        };
        opcode::Close::new(types::Fd(fd)).build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({
            macos_syscall!(match self.io_handle {
                OsRawHandle::Fd(fd) => libc::close(fd),
            })
        })
    }
}

#[cfg(windows)]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::{Foundation::CloseHandle, Networking::WinSock::closesocket};

        windows_syscall_submit!({
            match self.io_handle {
                OsRawHandle::Handle(handle) => windows_syscall!(BOOL, CloseHandle(handle as _)),
                OsRawHandle::Socket(socket) => windows_syscall!(SOCKET, closesocket(socket as _)),
            }
        })
    }
}

impl Completable for Close {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        // If the cancel op is successful we don't have to do anything else for it
        let _ = cqe.result?;

        Ok(())
    }
}
