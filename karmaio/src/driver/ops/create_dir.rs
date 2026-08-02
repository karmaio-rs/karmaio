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

/// Create a directory at path relative to the current working directory
/// of the caller's process.
pub(crate) struct CreateDir {
    pub(crate) path: OsPath,
    #[cfg(unix)]
    mode: libc::mode_t,
}

impl Op<CreateDir> {
    /// Submit a request to create a directory
    #[cfg(unix)]
    pub(crate) fn create_dir(path: &Path, mode: u32) -> std::io::Result<Op<CreateDir>> {
        let path = cstr(path)?;
        let data = CreateDir { path, mode: mode as _ };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn create_dir(path: &Path) -> std::io::Result<Op<CreateDir>> {
        let path = cstr(path)?;
        let data = CreateDir { path };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for CreateDir {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};
        let p_ref = self.path.as_c_str().as_ptr();

        opcode::MkDirAt::new(types::Fd(libc::AT_FDCWD), p_ref)
            .mode(self.mode)
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
impl KqueueOperation for CreateDir {
    type Output = std::io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let path = self.path.clone();
        let mode = self.mode;
        kqueue_syscall_blocking!({ kqueue_syscall!(libc::mkdir(path.as_c_str().as_ptr(), mode)) })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for CreateDir {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let path = self.path.clone();
        windows_syscall_blocking!({ windows_syscall!(BOOL, CreateDirectoryW(path.as_ptr(), std::ptr::null_mut())) })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
