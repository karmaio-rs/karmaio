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
        opcode::Close::new(types::Fd(self.io_handle.raw_fd())).build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        loop {
            let result = match self.io_handle {
                OsRawHandle::Fd(fd) => unsafe { libc::close(fd) },
            };

            // The fd has been closed. Return success completion
            if result == 0 {
                return Submission::Ready(Completion {
                    result: Ok(0),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();
            // The syscall was interrupted. Try again till we get success or error
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            return Submission::Ready(Completion {
                result: Err(err),
                flags: 0,
            });
        }
    }
}

#[cfg(windows)]
impl Submittable for Close {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            Networking::WinSock::{WSAGetLastError, closesocket},
        };

        let result = match self.io_handle {
            OsRawHandle::Handle(handle) => {
                if unsafe { CloseHandle(handle as _) } != 0 {
                    Ok(0)
                } else {
                    Err(io::Error::last_os_error())
                }
            }
            OsRawHandle::Socket(socket) => {
                if unsafe { closesocket(socket as _) } == 0 {
                    Ok(0)
                } else {
                    Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
                }
            }
        };

        Submission::Ready(Completion { result, flags: 0 })
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
