use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{cstr, OsPath};
use crate::driver::ops::{Completable, Completion, Op, Operable, Submittable};
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

impl Operable for Symlink {}

#[cfg(target_os = "linux")]
impl Submittable for Symlink {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::SymlinkAt::new(
            types::Fd(libc::AT_FDCWD),
            self.original.as_c_str().as_ptr(),
            self.link.as_c_str().as_ptr(),
        )
        .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Symlink {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({
            macos_syscall!(libc::symlink(
                self.original.as_c_str().as_ptr(),
                self.link.as_c_str().as_ptr(),
            ))
        })
    }
}

#[cfg(windows)]
impl Submittable for Symlink {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

        let flags = if self.dir {
            windows_sys::Win32::Storage::FileSystem::SYMBOLIC_LINK_FLAG_DIRECTORY
        } else {
            0
        };

        windows_syscall_submit!({
            windows_syscall!(BOOLEAN, CreateSymbolicLinkW(self.link.as_ptr(), self.original.as_ptr(), flags))
        })
    }
}

impl Completable for Symlink {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
