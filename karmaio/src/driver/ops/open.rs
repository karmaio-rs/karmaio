use std::{io, path::Path};

#[cfg(windows)]
use std::os::windows::io::RawHandle;

use crate::{
    driver::{
        Submission,
        helpers::{cstr::{cstr, OsPath}, io_handle::SharedIoHandle},
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    fs::{File, OpenOptions},
    runtime::local::CURRENT_DRIVER,
};

/// Open a file
pub(crate) struct Open {
    pub(crate) path: OsPath,
    options: OpenOptions,
    #[cfg(windows)]
    handle: Option<RawHandle>,
}

impl Op<Open> {
    pub(crate) fn open(path: &Path, options: OpenOptions) -> std::io::Result<Op<Open>> {
        // Validate option combinations early. This is required because the Linux
        // io_uring submit path returns a bare Entry and cannot use `?` to surface errors.
        #[cfg(unix)]
        {
            let _ = options.access_mode()?;
            let _ = options.creation_mode()?;
        }
        #[cfg(windows)]
        {
            let _ = options.access_mode()?;
            let _ = options.creation_mode()?;
        }

        let path = cstr(path)?;

        let data = Open {
            path,
            options,
            #[cfg(windows)]
            handle: None,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Open {}

#[cfg(target_os = "linux")]
impl Submittable for Open {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        let flags = libc::O_CLOEXEC
            | self.options.access_mode().expect("invalid open options combination")
            | self.options.creation_mode().expect("invalid open options combination")
            | (self.options.custom_flags & !libc::O_ACCMODE);

        let p_ref = self.path.as_c_str().as_ptr();

        opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), p_ref)
            .flags(flags)
            .mode(self.options.mode)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Open {
    fn submit(&mut self) -> Submission {
        let access_mode = match self.options.access_mode() {
            Ok(m) => m,
            Err(e) => {
                return Submission::Ready(Completion {
                    result: Err(e),
                    flags: 0,
                });
            }
        };
        let creation_mode = match self.options.creation_mode() {
            Ok(m) => m,
            Err(e) => {
                return Submission::Ready(Completion {
                    result: Err(e),
                    flags: 0,
                });
            }
        };

        let flags = libc::O_CLOEXEC | access_mode | creation_mode | (self.options.custom_flags & !libc::O_ACCMODE);

        loop {
            let fd = unsafe { libc::open(self.path.as_c_str().as_ptr(), flags, self.options.mode as u32) };

            if fd >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(fd as u32),
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
impl Submittable for Open {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::CreateFileW;

        let access_mode = match self.options.access_mode() {
            Ok(m) => m,
            Err(e) => {
                return Submission::Ready(Completion {
                    result: Err(e),
                    flags: 0,
                });
            }
        };
        let creation_mode = match self.options.creation_mode() {
            Ok(m) => m,
            Err(e) => {
                return Submission::Ready(Completion {
                    result: Err(e),
                    flags: 0,
                });
            }
        };

        let handle = unsafe {
            CreateFileW(
                self.path.as_ptr(),
                access_mode,
                self.options.share_mode,
                std::ptr::null_mut(),
                creation_mode,
                self.options.get_flags_and_attributes(),
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Submission::Ready(Completion {
                result: Err(io::Error::last_os_error()),
                flags: 0,
            });
        }

        self.handle = Some(handle as _);

        Submission::Ready(Completion {
            result: Ok(0),
            flags: 0,
        })
    }
}

#[cfg(windows)]
impl Drop for Open {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle as _);
            }
        }
    }
}

#[cfg(unix)]
impl Completable for Open {
    type Result = std::io::Result<File>;

    fn complete(self, cqe: Completion) -> Self::Result {
        Ok(File::from(SharedIoHandle::new(cqe.result? as _)))
    }
}

#[cfg(windows)]
impl Completable for Open {
    type Result = std::io::Result<File>;

    fn complete(mut self, cqe: Completion) -> Self::Result {
        let _ = cqe.result?;
        let handle = self.handle.take().expect("Open handle not set");
        Ok(File::from(SharedIoHandle::new_file(handle)))
    }
}
