use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{cstr, OsPath};
use crate::driver::ops::{Completable, Completion, Op, Operable, Submittable};
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

impl Operable for Rename {}

#[cfg(target_os = "linux")]
impl Submittable for Rename {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::RenameAt::new(
            types::Fd(libc::AT_FDCWD),
            self.from.as_c_str().as_ptr(),
            types::Fd(libc::AT_FDCWD),
            self.to.as_c_str().as_ptr(),
        )
        .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Rename {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({
            macos_syscall!(libc::rename(
                self.from.as_c_str().as_ptr(),
                self.to.as_c_str().as_ptr(),
            ))
        })
    }
}

#[cfg(windows)]
impl Submittable for Rename {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

        windows_syscall_submit!({
            windows_syscall!(BOOL, {
                MoveFileExW(
                    self.from.as_ptr(),
                    self.to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            })
        })
    }
}

impl Completable for Rename {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}