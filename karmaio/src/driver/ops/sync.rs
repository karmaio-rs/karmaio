use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Sync {
    handle: SharedIoHandle<std::fs::File>,
    #[allow(dead_code)] // This only works on linux. Macos and Windows do no support this
    sync_data: bool,
}

impl Op<Sync> {
    pub(crate) fn sync(handle: &SharedIoHandle<std::fs::File>) -> std::io::Result<Op<Sync>> {
        let data = Sync {
            handle: handle.clone(),
            sync_data: false,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }

    pub(crate) fn sync_data(handle: &SharedIoHandle<std::fs::File>) -> std::io::Result<Op<Sync>> {
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
        // Capture a raw fd (Send); SharedIoHandle stays on the op for the lifetime of the future.
        let fd = self.handle.raw_fd();
        macos_syscall_blocking!({ macos_syscall!(libc::fsync(fd)) })
    }
}

#[cfg(windows)]
impl Submittable for Sync {
    fn submit(&mut self) -> Submission {
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

        let handle = self.handle.raw_handle() as isize;
        windows_syscall_blocking!({ windows_syscall!(BOOL, FlushFileBuffers(handle as _)) })
    }
}

impl Completable for Sync {
    type Result = std::io::Result<()>;

    fn complete(self, cqe: Completion) -> Self::Result {
        cqe.result.map(|_| ())
    }
}
