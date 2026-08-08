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

use crate::{
    driver::{
        helpers::{
            attached_handle::AttachedHandle,
            cstr::{OsPath, cstr},
        },
        ops::{Completion, Op},
    },
    fs::{File, OpenOptions},
    runtime::local::CURRENT_DRIVER,
};

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;

/// Open a file
pub(crate) struct Open {
    pub(crate) path: OsPath,
    options: OpenOptions,
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

        let data = Open { path, options };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Open {
    type Output = std::io::Result<File>;
    fn submit(&mut self) -> UringSubmission {
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

    fn complete(self, cqe: Completion) -> Self::Output {
        // Safety: open returned a new open file descriptor; ownership transfers here.
        let file = unsafe { std::fs::File::from_raw_fd(cqe.result? as _) };
        Ok(File {
            handle: AttachedHandle::new(file)?,
        })
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Open {
    type Output = std::io::Result<File>;
    fn attempt(&mut self) -> KqueueAttempt {
        use rustix::fs::{Mode, OFlags};

        let access_mode = match self.options.access_mode() {
            Ok(m) => m,
            Err(e) => {
                return KqueueAttempt::Ready(Completion::new(Err(e)));
            }
        };
        let creation_mode = match self.options.creation_mode() {
            Ok(m) => m,
            Err(e) => {
                return KqueueAttempt::Ready(Completion::new(Err(e)));
            }
        };

        let flags = OFlags::CLOEXEC
            | OFlags::from_bits_retain(
                (access_mode | creation_mode | (self.options.custom_flags & !libc::O_ACCMODE)) as _,
            );
        let path = self.path.clone();
        let mode = Mode::from_raw_mode(self.options.mode as _);

        kqueue_syscall_blocking!(value {
            rustix::fs::open(path.as_c_str(), flags, mode)
                .map(std::fs::File::from)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        let file = completion.into_blocking_value::<std::fs::File>()?;
        Ok(File {
            handle: AttachedHandle::new(file)?,
        })
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Open {
    type Output = std::io::Result<File>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::CreateFileW;

        let access_mode = self.options.access_mode().expect("invalid open options combination");
        let creation_mode = self.options.creation_mode().expect("invalid open options combination");
        let path = self.path.clone();
        let share_mode = self.options.share_mode;
        let flags_and_attributes = self.options.get_flags_and_attributes();

        windows_syscall_blocking!(value {
            // Safety: `path` is an owned, NUL-terminated UTF-16 buffer that
            // remains alive for the call; the optional pointer arguments are null.
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    access_mode,
                    share_mode,
                    std::ptr::null_mut(),
                    creation_mode,
                    flags_and_attributes,
                    std::ptr::null_mut(),
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                Err(std::io::Error::last_os_error())
            } else {
                // Safety: `CreateFileW` returned a new owned file handle.
                Ok(unsafe { std::fs::File::from_raw_handle(handle) })
            }
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        let file = completion.into_blocking_value::<std::fs::File>()?;
        Ok(File {
            handle: AttachedHandle::new(file)?,
        })
    }
}
