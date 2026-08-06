use std::io;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
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
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    fs::Permissions,
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct SetPermissions {
    handle: SharedIoHandle<std::fs::File>,
    perm: Permissions,
}

impl Op<SetPermissions> {
    pub(crate) fn set_permissions(
        handle: &SharedIoHandle<std::fs::File>,
        perm: Permissions,
    ) -> io::Result<Op<SetPermissions>> {
        let data = SetPermissions {
            handle: handle.clone(),
            perm,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for SetPermissions {
    type Output = io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        let fd = self.handle.raw_fd();
        let mode = self.perm.mode();
        kqueue_syscall_blocking!({
            // Safety: the operation retains the owning file handle until this
            // blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            rustix::fs::fchmod(fd, rustix::fs::Mode::from_raw_mode(mode as _))
                .map(|()| 0_u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for SetPermissions {
    type Output = io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
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

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
