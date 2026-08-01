use std::io;
use std::path::Path;

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op};
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

#[cfg(target_os = "linux")]
impl BackendSubmit for Unlink {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};

        let flags = if self.remove_dir { libc::AT_REMOVEDIR } else { 0 };

        opcode::UnlinkAt::new(types::Fd(libc::AT_FDCWD), self.path.as_c_str().as_ptr())
            .flags(flags)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl BackendSubmit for Unlink {
    fn submit(&mut self) -> BackendSubmission {
        let path = self.path.clone();
        let remove_dir = self.remove_dir;
        macos_syscall_blocking!({
            macos_syscall!(if remove_dir {
                libc::rmdir(path.as_c_str().as_ptr())
            } else {
                libc::unlink(path.as_c_str().as_ptr())
            })
        })
    }
}

#[cfg(windows)]
impl BackendSubmit for Unlink {
    fn submit(&mut self) -> BackendSubmission {
        use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, RemoveDirectoryW};

        let path = self.path.clone();
        let remove_dir = self.remove_dir;
        windows_syscall_blocking!({
            if remove_dir {
                windows_syscall!(BOOL, RemoveDirectoryW(path.as_ptr()))
            } else {
                windows_syscall!(BOOL, DeleteFileW(path.as_ptr()))
            }
        })
    }
}

impl BackendComplete for Unlink {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
