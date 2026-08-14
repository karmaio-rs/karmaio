use std::io;

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
    buf::{BufResult, IoVectoredBuf},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Writev<V: IoVectoredBuf> {
    #[allow(unused)]
    io_handle: SharedIoHandle<std::fs::File>,
    pub(crate) bufs: V,
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,
    offset: u64,
}

impl<V: IoVectoredBuf> Op<Writev<V>> {
    pub(crate) fn writev(io_handle: &SharedIoHandle<std::fs::File>, bufs: V, offset: u64) -> io::Result<Op<Writev<V>>> {
        let data = Writev {
            io_handle: io_handle.clone(),
            bufs,
            #[cfg(unix)]
            iovs: Vec::new(),
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<V: IoVectoredBuf> Writev<V> {
    #[cfg(unix)]
    fn rebuild_iovs(&mut self) {
        self.iovs = self
            .bufs
            .iter_slice()
            .map(|buf| libc::iovec {
                iov_base: buf.as_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            })
            .collect();
    }

    fn finish(self, completion: Completion) -> BufResult<usize, V> {
        BufResult(completion.result.map(|n| n as usize), self.bufs)
    }
}

#[cfg(target_os = "linux")]
unsafe impl<V: IoVectoredBuf> UringOperation for Writev<V> {
    type Output = BufResult<usize, V>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        self.rebuild_iovs();
        opcode::Writev::new(
            types::Fd(self.io_handle.raw_fd()),
            self.iovs.as_ptr(),
            self.iovs.len() as u32,
        )
        .offset(self.offset as _)
        .build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<V: IoVectoredBuf> KqueueOperation for Writev<V> {
    type Output = BufResult<usize, V>;

    fn attempt(&mut self) -> KqueueAttempt {
        self.rebuild_iovs();
        let fd = self.io_handle.raw_fd();
        let iovs = self.iovs.as_ptr() as usize;
        let iovcnt = self.iovs.len();
        let offset = self.offset;
        kqueue_syscall_blocking!({
            // Safety: the stable operation carrier retains the descriptor
            // array, buffers, and file until the blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let iovs = unsafe { std::slice::from_raw_parts(iovs as *const libc::iovec, iovcnt) };
            let bufs = iovs
                .iter()
                .map(|iov| {
                    let buf = unsafe { std::slice::from_raw_parts(iov.iov_base.cast::<u8>(), iov.iov_len) };
                    std::io::IoSlice::new(buf)
                })
                .collect::<Vec<_>>();
            rustix::io::pwritev(fd, &bufs, offset)
                .map(|n| n as u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}
