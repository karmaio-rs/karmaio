use std::io;
use std::path::Path;

use crate::driver::Submission;
use crate::driver::helpers::cstr::{cstr, OsPath};
use crate::driver::ops::{Completable, Completion, Op, Operable, Submittable};
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

impl Operable for Hardlink {}

#[cfg(target_os = "linux")]
impl Submittable for Hardlink {
    fn submit(&mut self) -> Submission {
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
impl Submittable for Hardlink {
    fn submit(&mut self) -> Submission {
        loop {
            let ret = unsafe { libc::link(self.original.as_c_str().as_ptr(), self.link.as_c_str().as_ptr()) };

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
impl Submittable for Hardlink {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::CreateHardLinkW;

        let result = unsafe { CreateHardLinkW(self.link.as_ptr(), self.original.as_ptr(), std::ptr::null_mut()) };

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

impl Completable for Hardlink {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}