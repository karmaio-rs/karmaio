use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{OsPath, cstr};
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
        let data = Unlink {
            path,
            remove_dir: false,
        };

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
        macos_syscall_submit!({
            macos_syscall!(if self.remove_dir {
                libc::rmdir(self.path.as_c_str().as_ptr())
            } else {
                libc::unlink(self.path.as_c_str().as_ptr())
            })
        })
    }
}

#[cfg(windows)]
impl Submittable for Unlink {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, RemoveDirectoryW};

        windows_syscall_submit!({
            if self.remove_dir {
                windows_syscall!(BOOL, RemoveDirectoryW(self.path.as_ptr()))
            } else {
                windows_syscall!(BOOL, DeleteFileW(self.path.as_ptr()))
            }
        })
    }
}

impl Completable for Unlink {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
