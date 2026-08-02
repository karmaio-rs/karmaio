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

/// Rename a file or directory on the filesystem.
pub(crate) struct Rename {
    pub(crate) from: OsPath,
    pub(crate) to: OsPath,
}

impl Op<Rename> {
    pub(crate) fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<Op<Rename>> {
        let from = cstr(from.as_ref())?;
        let to = cstr(to.as_ref())?;

        let data = Rename { from, to };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Rename {
    type Output = io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::RenameAt::new(
            types::Fd(libc::AT_FDCWD),
            self.from.as_c_str().as_ptr(),
            types::Fd(libc::AT_FDCWD),
            self.to.as_c_str().as_ptr(),
        )
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
impl KqueueOperation for Rename {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let from = self.from.clone();
        let to = self.to.clone();
        kqueue_syscall_blocking!({
            rustix::fs::rename(from.as_c_str(), to.as_c_str())
                .map(|()| 0_u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Rename {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let from = self.from.clone();
        let to = self.to.clone();
        windows_syscall_blocking!({
            windows_syscall!(BOOL, {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            })
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
