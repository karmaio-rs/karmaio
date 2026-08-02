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
unsafe impl UringOperation for Hardlink {
    type Output = io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::LinkAt::new(
            types::Fd(libc::AT_FDCWD),
            self.original.as_c_str().as_ptr(),
            types::Fd(libc::AT_FDCWD),
            self.link.as_c_str().as_ptr(),
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
impl KqueueOperation for Hardlink {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let original = self.original.clone();
        let link = self.link.clone();
        kqueue_syscall_blocking!({
            rustix::fs::link(original.as_c_str(), link.as_c_str())
                .map(|()| 0_u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Hardlink {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
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

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
