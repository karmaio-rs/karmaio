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
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
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

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Sync {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let mut op = opcode::Fsync::new(types::Fd(self.handle.raw_fd()));

        if self.sync_data {
            op = op.flags(types::FsyncFlags::DATASYNC);
        }

        op.build()
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Sync {
    type Output = std::io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        // Capture a raw fd (Send); SharedIoHandle stays on the op for the lifetime of the future.
        let fd = self.handle.raw_fd();
        kqueue_syscall_blocking!({ kqueue_syscall!(libc::fsync(fd)) })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Sync {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

        let handle = self.handle.raw_handle() as isize;
        windows_syscall_blocking!({ windows_syscall!(BOOL, FlushFileBuffers(handle as _)) })
    }

    fn complete(self, cqe: Completion) -> Self::Output {
        cqe.result.map(|_| ())
    }
}
