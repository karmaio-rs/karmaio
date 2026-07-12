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
        // Own the raw fd for the pool job (Close owns the handle exclusively).
        let fd = match self.io_handle {
            OsRawHandle::Fd(fd) => fd,
        };
        macos_syscall_blocking!({ macos_syscall!(libc::close(fd)) })
    }
}

#[cfg(windows)]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::{Foundation::CloseHandle, Networking::WinSock::closesocket};

        // Copy discriminant values for the Send job.
        let handle = self.io_handle;
        windows_syscall_blocking!({
            match handle {
                OsRawHandle::Handle(h) => windows_syscall!(BOOL, CloseHandle(h as _)),
                OsRawHandle::Socket(s) => windows_syscall!(SOCKET, closesocket(s as _)),
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
