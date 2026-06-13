use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{cstr, OsPath};
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
        loop {
            let ret = unsafe { libc::mkdir(self.path.as_c_str().as_ptr(), self.mode) };

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
impl Submittable for CreateDir {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let result = unsafe { CreateDirectoryW(self.path.as_ptr(), std::ptr::null_mut()) };

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

impl Completable for CreateDir {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
