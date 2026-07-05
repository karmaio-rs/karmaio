use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{Completable, Completion, Op, Operable, Submittable};
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

impl Operable for CreateDir {}

#[cfg(target_os = "linux")]
impl Submittable for CreateDir {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};
        let p_ref = self.path.as_c_str().as_ptr();

        opcode::MkDirAt::new(types::Fd(libc::AT_FDCWD), p_ref)
            .mode(self.mode)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for CreateDir {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({ macos_syscall!(libc::mkdir(self.path.as_c_str().as_ptr(), self.mode)) })
    }
}

#[cfg(windows)]
impl Submittable for CreateDir {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        windows_syscall_submit!({ windows_syscall!(BOOL, CreateDirectoryW(self.path.as_ptr(), std::ptr::null_mut())) })
    }
}

impl Completable for CreateDir {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
