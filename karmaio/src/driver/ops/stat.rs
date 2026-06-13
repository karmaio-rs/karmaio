use std::io;

use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    fs::Metadata,
    runtime::local::CURRENT_DRIVER,
};

#[cfg(windows)]
use crate::driver::helpers::io_handle::OsRawHandle;

pub(crate) struct Stat {
    handle: SharedIoHandle,
    #[cfg(target_os = "linux")]
    statx_buf: Box<libc::statx>,
    #[cfg(target_os = "macos")]
    stat_buf: Option<libc::stat>,
    #[cfg(windows)]
    result: Option<Metadata>,
}

impl Op<Stat> {
    pub(crate) fn stat(handle: &SharedIoHandle) -> io::Result<Op<Stat>> {
        #[cfg(target_os = "linux")]
        let data = Stat {
            handle: handle.clone(),
            statx_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        #[cfg(target_os = "macos")]
        let data = Stat {
            handle: handle.clone(),
            stat_buf: None,
        };

        #[cfg(windows)]
        let data = Stat {
            handle: handle.clone(),
            result: None,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Stat {}

#[cfg(target_os = "linux")]
impl Submittable for Stat {
    fn submit(&mut self) -> Submission {
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
}

#[cfg(target_os = "macos")]
impl Submittable for Stat {
    fn submit(&mut self) -> Submission {
        loop {
            let mut stat = unsafe { std::mem::zeroed() };
            let ret = unsafe { libc::fstat(self.handle.raw_fd(), &mut stat) };

            if ret == 0 {
                self.stat_buf = Some(stat);
                return Submission::Ready(Completion {
                    result: Ok(0),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::Interrupted {
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
impl Submittable for Stat {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Foundation::HANDLE;

        match self.handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => match Metadata::from_handle(handle as HANDLE) {
                Ok(metadata) => {
                    self.result = Some(metadata);
                    Submission::Ready(Completion {
                        result: Ok(0),
                        flags: 0,
                    })
                }
                Err(err) => Submission::Ready(Completion {
                    result: Err(err),
                    flags: 0,
                }),
            },
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cannot query metadata for a socket",
                )),
                flags: 0,
            }),
        }
    }
}

#[cfg(target_os = "linux")]
impl Completable for Stat {
    type Result = io::Result<Metadata>;

    fn complete(self, completion: Completion) -> Self::Result {
        completion.result?;
        Ok(Metadata::from_statx(*self.statx_buf))
    }
}

#[cfg(target_os = "macos")]
impl Completable for Stat {
    type Result = io::Result<Metadata>;

    fn complete(self, completion: Completion) -> Self::Result {
        completion.result?;
        let stat = self.stat_buf.expect("fstat result missing after successful submit");
        Ok(Metadata::from_stat(stat))
    }
}

#[cfg(windows)]
impl Completable for Stat {
    type Result = io::Result<Metadata>;

    fn complete(self, completion: Completion) -> Self::Result {
        completion.result?;
        Ok(self.result.expect("metadata missing after successful submit"))
    }
}
