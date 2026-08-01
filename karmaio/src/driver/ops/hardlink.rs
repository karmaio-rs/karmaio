use std::io;
use std::path::Path;

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op};
use crate::runtime::local::CURRENT_DRIVER;

/// Create a hard link on the filesystem.
pub(crate) struct Hardlink {
    pub(crate) original: OsPath,
    pub(crate) link: OsPath,
}

impl Op<Hardlink> {
    pub(crate) fn hardlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<Op<Hardlink>> {
        let original = cstr(original.as_ref())?;
        let link = cstr(link.as_ref())?;

        let data = Hardlink { original, link };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
impl BackendSubmit for Hardlink {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};

        opcode::LinkAt::new(
            types::Fd(libc::AT_FDCWD),
            self.original.as_c_str().as_ptr(),
            types::Fd(libc::AT_FDCWD),
            self.link.as_c_str().as_ptr(),
        )
        .build()
    }
}

#[cfg(target_os = "macos")]
impl BackendSubmit for Hardlink {
    fn submit(&mut self) -> BackendSubmission {
        let original = self.original.clone();
        let link = self.link.clone();
        macos_syscall_blocking!({ macos_syscall!(libc::link(original.as_c_str().as_ptr(), link.as_c_str().as_ptr(),)) })
    }
}

#[cfg(windows)]
impl BackendSubmit for Hardlink {
    fn submit(&mut self) -> BackendSubmission {
        use windows_sys::Win32::Storage::FileSystem::CreateHardLinkW;

        let original = self.original.clone();
        let link = self.link.clone();
        windows_syscall_blocking!({
            windows_syscall!(
                BOOL,
                CreateHardLinkW(link.as_ptr(), original.as_ptr(), std::ptr::null_mut())
            )
        })
    }
}

impl BackendComplete for Hardlink {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
