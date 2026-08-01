use std::path::Path;

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op};
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
impl BackendSubmit for CreateDir {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};
        let p_ref = self.path.as_c_str().as_ptr();

        opcode::MkDirAt::new(types::Fd(libc::AT_FDCWD), p_ref)
            .mode(self.mode)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl BackendSubmit for CreateDir {
    fn submit(&mut self) -> BackendSubmission {
        let path = self.path.clone();
        let mode = self.mode;
        macos_syscall_blocking!({ macos_syscall!(libc::mkdir(path.as_c_str().as_ptr(), mode)) })
    }
}

#[cfg(windows)]
impl BackendSubmit for CreateDir {
    fn submit(&mut self) -> BackendSubmission {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let path = self.path.clone();
        windows_syscall_blocking!({ windows_syscall!(BOOL, CreateDirectoryW(path.as_ptr(), std::ptr::null_mut())) })
    }
}

impl BackendComplete for CreateDir {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
