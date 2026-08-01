use std::io;

use crate::{
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op},
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
impl BackendSubmit for Truncate {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};

        opcode::Ftruncate::new(types::Fd(self.handle.raw_fd()), self.size).build()
    }
}

#[cfg(target_os = "macos")]
impl BackendSubmit for Truncate {
    fn submit(&mut self) -> BackendSubmission {
        let fd = self.handle.raw_fd();
        let size = self.size as libc::off_t;
        macos_syscall_blocking!({ macos_syscall!(libc::ftruncate(fd, size)) })
    }
}

#[cfg(windows)]
impl BackendSubmit for Truncate {
    fn submit(&mut self) -> BackendSubmission {
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
}

impl BackendComplete for Truncate {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
