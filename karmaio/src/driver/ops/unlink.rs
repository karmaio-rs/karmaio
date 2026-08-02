use std::io;
use std::path::Path;

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

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{Completion, Op};
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
unsafe impl UringOperation for Unlink {
    type Output = io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let flags = if self.remove_dir { libc::AT_REMOVEDIR } else { 0 };

        opcode::UnlinkAt::new(types::Fd(libc::AT_FDCWD), self.path.as_c_str().as_ptr())
            .flags(flags)
            .build()
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Unlink {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let path = self.path.clone();
        let remove_dir = self.remove_dir;
        kqueue_syscall_blocking!({
            kqueue_syscall!(if remove_dir {
                libc::rmdir(path.as_c_str().as_ptr())
            } else {
                libc::unlink(path.as_c_str().as_ptr())
            })
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Unlink {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
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

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
