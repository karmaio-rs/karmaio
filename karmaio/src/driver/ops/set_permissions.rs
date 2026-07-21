use std::io;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    fs::Permissions,
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct SetPermissions {
    handle: SharedIoHandle<std::fs::File>,
    perm: Permissions,
    #[cfg(target_os = "linux")]
    result: Option<io::Result<()>>,
}

impl Op<SetPermissions> {
    pub(crate) fn set_permissions(
        handle: &SharedIoHandle<std::fs::File>,
        perm: Permissions,
    ) -> io::Result<Op<SetPermissions>> {
        let data = SetPermissions {
            handle: handle.clone(),
            perm,
            #[cfg(target_os = "linux")]
            result: None,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for SetPermissions {}

#[cfg(target_os = "macos")]
impl Submittable for SetPermissions {
    fn submit(&mut self) -> Submission {
        let fd = self.handle.raw_fd();
        let mode = self.perm.mode() as libc::mode_t;
        macos_syscall_blocking!({ macos_syscall!(libc::fchmod(fd, mode)) })
    }
}

#[cfg(target_os = "linux")]
impl Submittable for SetPermissions {
    fn submit(&mut self) -> Submission {
        use io_uring::opcode;

        // No direct fchmod opcode in io_uring (as of current version). Perform
        // the operation synchronously here (like kqueue path) and deliver the
        // result via a NOP submission so the uring backend can track it.
        loop {
            let ret = unsafe { libc::fchmod(self.handle.raw_fd(), self.perm.mode() as libc::mode_t) };

            if ret == 0 {
                self.result = Some(Ok(()));
                return opcode::Nop::new().build();
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }

            self.result = Some(Err(err));
            return opcode::Nop::new().build();
        }
    }
}

#[cfg(windows)]
impl Submittable for SetPermissions {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::{FileBasicInfo, SetFileInformationByHandle};

        // Mirrors `FILE_BASIC_INFORMATION` from the Windows SDK. `windows-sys` does not
        // expose this struct directly, so it is defined here.
        #[repr(C)]
        struct FileBasicInformation {
            creation_time: i64,
            last_access_time: i64,
            last_write_time: i64,
            change_time: i64,
            file_attributes: u32,
        }

        // Setting only `file_attributes` (with the time fields zeroed) asks
        // Windows to update the attributes and leave the timestamps alone.
        let handle = self.handle.raw_handle() as isize;
        let attrs = self.perm.attrs();

        windows_syscall_blocking!({
            let info = FileBasicInformation {
                creation_time: 0,
                last_access_time: 0,
                last_write_time: 0,
                change_time: 0,
                file_attributes: attrs,
            };
            windows_syscall!(BOOL, {
                SetFileInformationByHandle(
                    handle as _,
                    FileBasicInfo,
                    &info as *const _ as *const _,
                    std::mem::size_of::<FileBasicInformation>() as u32,
                )
            })
        })
    }
}

#[cfg(target_os = "linux")]
impl Completable for SetPermissions {
    type Result = io::Result<()>;

    fn complete(self, _cqe: Completion) -> Self::Result {
        self.result
            .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::Other, "set_permissions result missing")))
    }
}

#[cfg(not(target_os = "linux"))]
impl Completable for SetPermissions {
    type Result = io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
