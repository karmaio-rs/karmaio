use std::io;

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
    fs::Metadata,
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Stat {
    handle: SharedIoHandle<std::fs::File>,
    #[cfg(target_os = "linux")]
    statx_buf: Box<libc::statx>,
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    /// Filled by the blocking-pool job via shared storage.
    stat_shared: Option<std::sync::Arc<std::sync::Mutex<Option<libc::stat>>>>,
    #[cfg(windows)]
    result: Option<Metadata>,
}

impl Op<Stat> {
    pub(crate) fn stat(handle: &SharedIoHandle<std::fs::File>) -> io::Result<Op<Stat>> {
        #[cfg(target_os = "linux")]
        let data = Stat {
            handle: handle.clone(),
            statx_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        let data = Stat {
            handle: handle.clone(),
            stat_shared: None,
        };

        #[cfg(windows)]
        let data = Stat {
            handle: handle.clone(),
            result: None,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Stat {
    type Output = io::Result<Metadata>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let buf_ptr = self.statx_buf.as_mut() as *mut libc::statx as *mut types::statx;

        // Use AT_EMPTY_PATH with an empty path to stat the file descriptor itself.
        // Passing a null path without the flag is invalid for fd-based statx.
        let empty: *const libc::c_char = b"\0".as_ptr() as *const _;

        opcode::Statx::new(types::Fd(self.handle.raw_fd()), empty, buf_ptr)
            .flags(libc::AT_EMPTY_PATH)
            .mask(libc::STATX_BASIC_STATS | libc::STATX_BTIME)
            .build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        Ok(Metadata::from_statx(*self.statx_buf))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Stat {
    type Output = io::Result<Metadata>;
    fn attempt(&mut self) -> KqueueAttempt {
        use std::sync::{Arc, Mutex};

        // fstat fills a buffer we need after the pool job; share it with the worker.
        let slot = Arc::new(Mutex::new(None::<libc::stat>));
        self.stat_shared = Some(Arc::clone(&slot));
        let fd = self.handle.raw_fd();

        kqueue_syscall_blocking!({
            let mut stat = unsafe { std::mem::zeroed() };
            let result = kqueue_syscall!(libc::fstat(fd, &mut stat));
            if result.is_ok() {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(stat);
            }
            result
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        let slot = self
            .stat_shared
            .expect("fstat shared slot missing after successful submit");
        let stat = slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("fstat result missing after successful submit");
        Ok(Metadata::from_stat(stat))
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Stat {
    type Output = io::Result<Metadata>;
    fn submit(&mut self) -> IocpSubmission {
        use windows_sys::Win32::Foundation::HANDLE;

        match Metadata::from_handle(self.handle.raw_handle() as HANDLE) {
            Ok(metadata) => {
                self.result = Some(metadata);
                IocpSubmission::Ready(Completion { result: Ok(0) })
            }
            Err(err) => IocpSubmission::Ready(Completion { result: Err(err) }),
        }
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        Ok(self.result.expect("metadata missing after successful submit"))
    }
}
