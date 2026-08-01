use std::path::Path;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(target_os = "macos")]
use crate::driver::backends::kqueue::{PollAttempt, PollOperation};

use crate::driver::helpers::cstr::{OsPath, cstr};
use crate::driver::ops::{Completion, Op};
use crate::runtime::local::CURRENT_DRIVER;

/// Create a symbolic link on the filesystem.
pub(crate) struct Symlink {
    pub(crate) original: OsPath,
    pub(crate) link: OsPath,
    #[cfg(windows)]
    dir: bool,
}

impl Op<Symlink> {
    /// Submit a request to create a symbolic link
    #[cfg(unix)]
    pub(crate) fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<Op<Symlink>> {
        let original = cstr(original.as_ref())?;
        let link = cstr(link.as_ref())?;

        let data = Symlink { original, link };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    #[cfg(windows)]
    pub(crate) fn symlink_file<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<Op<Symlink>> {
        let original = cstr(original.as_ref())?;
        let link = cstr(link.as_ref())?;

        let data = Symlink {
            original,
            link,
            dir: false,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    #[cfg(windows)]
    pub(crate) fn symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<Op<Symlink>> {
        let original = cstr(original.as_ref())?;
        let link = cstr(link.as_ref())?;

        let data = Symlink {
            original,
            link,
            dir: true,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Symlink {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::SymlinkAt::new(
            types::Fd(libc::AT_FDCWD),
            self.original.as_c_str().as_ptr(),
            self.link.as_c_str().as_ptr(),
        )
        .build()
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(target_os = "macos")]
impl PollOperation for Symlink {
    type Output = std::io::Result<()>;
    fn attempt(&mut self) -> PollAttempt {
        let original = self.original.clone();
        let link = self.link.clone();
        macos_syscall_blocking!({
            macos_syscall!(libc::symlink(original.as_c_str().as_ptr(), link.as_c_str().as_ptr(),))
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Symlink {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

        let flags = if self.dir {
            windows_sys::Win32::Storage::FileSystem::SYMBOLIC_LINK_FLAG_DIRECTORY
        } else {
            0
        };

        let original = self.original.clone();
        let link = self.link.clone();
        windows_syscall_blocking!({
            windows_syscall!(BOOLEAN, CreateSymbolicLinkW(link.as_ptr(), original.as_ptr(), flags))
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
