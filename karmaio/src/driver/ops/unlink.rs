use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{cstr, OsPath};
use crate::driver::ops::{Completable, Completion, Op, Operable, Submittable};
use crate::runtime::local::CURRENT_DRIVER;

/// Remove a file or directory from the filesystem.
pub(crate) struct Unlink {
    pub(crate) path: OsPath,
    remove_dir: bool,
}

impl Op<Unlink> {
    pub(crate) fn remove_file(path: &Path) -> io::Result<Op<Unlink>> {
        let path = cstr(path)?;
        let data = Unlink { path, remove_dir: false };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    pub(crate) fn remove_dir(path: &Path) -> io::Result<Op<Unlink>> {
        let path = cstr(path)?;
        let data = Unlink { path, remove_dir: true };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Unlink {}

#[cfg(target_os = "linux")]
impl Submittable for Unlink {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        let flags = if self.remove_dir { libc::AT_REMOVEDIR } else { 0 };

        opcode::UnlinkAt::new(types::Fd(libc::AT_FDCWD), self.path.as_c_str().as_ptr())
            .flags(flags)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Unlink {
    fn submit(&mut self) -> Submission {
        loop {
            let ret = if self.remove_dir {
                unsafe { libc::rmdir(self.path.as_c_str().as_ptr()) }
            } else {
                unsafe { libc::unlink(self.path.as_c_str().as_ptr()) }
            };

            if ret == 0 {
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
impl Submittable for Unlink {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, RemoveDirectoryW};

        let result = if self.remove_dir {
            unsafe { RemoveDirectoryW(self.path.as_ptr()) }
        } else {
            unsafe { DeleteFileW(self.path.as_ptr()) }
        };

        if result != 0 {
            Submission::Ready(Completion {
                result: Ok(0),
                flags: 0,
            })
        } else {
            Submission::Ready(Completion {
                result: Err(io::Error::last_os_error()),
                flags: 0,
            })
        }
    }
}

impl Completable for Unlink {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}