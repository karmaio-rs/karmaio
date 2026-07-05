use std::io;

#[cfg(windows)]
use crate::driver::helpers::io_handle::OsRawHandle;
use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Truncate {
    handle: SharedIoHandle,
    size: u64,
}

impl Op<Truncate> {
    pub(crate) fn truncate(handle: &SharedIoHandle, size: u64) -> io::Result<Op<Truncate>> {
        let data = Truncate {
            handle: handle.clone(),
            size,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Truncate {}

#[cfg(target_os = "linux")]
impl Submittable for Truncate {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::Ftruncate::new(types::Fd(self.handle.raw_fd()), self.size).build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Truncate {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({ macos_syscall!(libc::ftruncate(self.handle.raw_fd(), self.size as libc::off_t)) })
    }
}

#[cfg(windows)]
impl Submittable for Truncate {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::{SetEndOfFile, SetFilePointerEx};

        match self.handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => {
                let distance_to_move: i64 = self.size as i64;
                let mut new_file_pointer: i64 = 0;

                windows_syscall_submit!({
                    match windows_syscall!(BOOL, {
                        SetFilePointerEx(
                            handle as _,
                            distance_to_move,
                            &mut new_file_pointer,
                            windows_sys::Win32::Storage::FileSystem::FILE_BEGIN,
                        )
                    }) {
                        Ok(val) => windows_syscall!(BOOL, SetEndOfFile(handle as _)),
                        Err(err) => Err(err),
                    }
                })
            }
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(io::Error::new(io::ErrorKind::Unsupported, "cannot truncate a socket")),
                flags: 0,
            }),
        }
    }
}

impl Completable for Truncate {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
