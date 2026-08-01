use std::path::Path;

#[cfg(windows)]
use std::os::windows::io::RawHandle;

use crate::{
    driver::{
        helpers::{
            attached_handle::AttachedHandle,
            cstr::{OsPath, cstr},
        },
        ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op},
    },
    fs::{File, OpenOptions},
    runtime::local::CURRENT_DRIVER,
};

#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;

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

        let op = CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))?;

        // On Windows, CreateFileW is synchronous but the handle must be
        // associated with the IOCP before any overlapped I/O is performed on it.
        #[cfg(target_os = "windows")]
        if let Some(open) = op.data_ref() {
            if let Some(handle) = open.handle {
                CURRENT_DRIVER.with(|driver| {
                    let driver = driver.upgrade().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "runtime is shutting down")
                    })?;
                    driver.attach(handle)
                })?;
            }
        }

        Ok(op)
    }
}

#[cfg(target_os = "linux")]
impl BackendSubmit for Open {
    fn submit(&mut self) -> BackendSubmission {
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
impl BackendSubmit for Open {
    fn submit(&mut self) -> BackendSubmission {
        let access_mode = match self.options.access_mode() {
            Ok(m) => m,
            Err(e) => {
                return BackendSubmission::Ready(Completion { result: Err(e) });
            }
        };
        let creation_mode = match self.options.creation_mode() {
            Ok(m) => m,
            Err(e) => {
                return BackendSubmission::Ready(Completion { result: Err(e) });
            }
        };

        let flags = libc::O_CLOEXEC | access_mode | creation_mode | (self.options.custom_flags & !libc::O_ACCMODE);
        let path = self.path.clone();
        let mode = self.options.mode as u32;

        macos_syscall_blocking!({ macos_syscall!(libc::open(path.as_c_str().as_ptr(), flags, mode)) })
    }
}

#[cfg(windows)]
impl BackendSubmit for Open {
    fn submit(&mut self) -> BackendSubmission {
        use windows_sys::Win32::Storage::FileSystem::CreateFileW;

        let access_mode = match self.options.access_mode() {
            Ok(m) => m,
            Err(e) => {
                return BackendSubmission::Ready(Completion { result: Err(e) });
            }
        };
        let creation_mode = match self.options.creation_mode() {
            Ok(m) => m,
            Err(e) => {
                return BackendSubmission::Ready(Completion { result: Err(e) });
            }
        };

        match windows_syscall!(HANDLE, {
            CreateFileW(
                self.path.as_ptr(),
                access_mode,
                self.options.share_mode,
                std::ptr::null_mut(),
                creation_mode,
                self.options.get_flags_and_attributes(),
                std::ptr::null_mut(),
            )
        }) {
            Ok(handle) => {
                self.handle = Some(handle as _);
                BackendSubmission::Ready(Completion { result: Ok(0) })
            }
            Err(err) => BackendSubmission::Ready(Completion { result: Err(err) }),
        }
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
impl BackendComplete for Open {
    type Result = std::io::Result<File>;

    fn complete(self, cqe: Completion) -> Self::Result {
        // Safety: open returned a new open file descriptor; ownership transfers here.
        let file = unsafe { std::fs::File::from_raw_fd(cqe.result? as _) };
        Ok(File {
            // SAFETY: The file was just opened and will be used within the runtime context.
            handle: unsafe { AttachedHandle::new_unchecked(file) },
        })
    }
}

#[cfg(windows)]
impl BackendComplete for Open {
    type Result = std::io::Result<File>;

    fn complete(mut self, cqe: Completion) -> Self::Result {
        let _ = cqe.result?;
        let handle = self.handle.take().expect("Open handle not set");
        // Safety: open produced an open Win32 file handle; ownership transfers here.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        Ok(File {
            // SAFETY: The file was just opened and will be used within the runtime context.
            handle: unsafe { AttachedHandle::new_unchecked(file) },
        })
    }
}
