use std::io;

#[cfg(windows)]
use crate::driver::helpers::io_handle::OsRawHandle;
use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Sync {
    handle: SharedIoHandle,
    #[allow(dead_code)] // This only works on linux. Macos and Windows do no support this
    sync_data: bool,
}

impl Op<Sync> {
    pub(crate) fn sync(handle: &SharedIoHandle) -> std::io::Result<Op<Sync>> {
        let data = Sync {
            handle: handle.clone(),
            sync_data: false,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    pub(crate) fn sync_data(handle: &SharedIoHandle) -> std::io::Result<Op<Sync>> {
        let data = Sync {
            handle: handle.clone(),
            sync_data: true,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Sync {}

#[cfg(target_os = "linux")]
impl Submittable for Sync {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        let mut op = opcode::Fsync::new(types::Fd(self.handle.raw_fd()));

        if self.sync_data {
            op = op.flags(types::FsyncFlags::DATASYNC);
        }

        op.build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Sync {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!({
            macos_syscall!(libc::fsync(self.handle.raw_fd()))
        })
    }
}

#[cfg(windows)]
impl Submittable for Sync {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

        match self.handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => windows_syscall_submit!({
                windows_syscall!(BOOL, FlushFileBuffers(handle as _))
            }),
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(io::Error::new(io::ErrorKind::Unsupported, "cannot sync a socket")),
                flags: 0,
            }),
        }
    }
}

impl Completable for Sync {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
