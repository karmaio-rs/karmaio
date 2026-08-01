use std::io;
use std::path::Path;

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op};
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
impl BackendSubmit for Rename {
    fn submit(&mut self) -> BackendSubmission {
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
impl BackendSubmit for Rename {
    fn submit(&mut self) -> BackendSubmission {
        let from = self.from.clone();
        let to = self.to.clone();
        macos_syscall_blocking!({ macos_syscall!(libc::rename(from.as_c_str().as_ptr(), to.as_c_str().as_ptr())) })
    }
}

#[cfg(windows)]
impl BackendSubmit for Rename {
    fn submit(&mut self) -> BackendSubmission {
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
}

impl BackendComplete for Rename {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
