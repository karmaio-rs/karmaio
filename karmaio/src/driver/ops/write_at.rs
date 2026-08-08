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
    buf::{BufResult, IoBuf},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct WriteAt<B: IoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle<std::fs::File>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Write offset
    offset: u64,
}

impl<B: IoBuf> Op<WriteAt<B>> {
    pub(crate) fn write_at(
        io_handle: &SharedIoHandle<std::fs::File>,
        buf: B,
        offset: u64,
    ) -> std::io::Result<Op<WriteAt<B>>> {
        let data = WriteAt {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: IoBuf> UringOperation for WriteAt<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.as_init().as_ptr();
        let len = self.buf.as_init().len();

        opcode::Write::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _)
            .offset(self.offset as _)
            .build()
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        BufResult(res, buf)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<B: IoBuf> KqueueOperation for WriteAt<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        // Regular files always report ready under kqueue, and pwrite can block on
        // disk I/O — offload to the blocking pool so the runtime thread stays free.
        // Buffer pointers remain valid: the Op (and its buffers) stay alive until
        // the pool job completes and `complete` runs.
        let fd = self.io_handle.raw_fd();
        let ptr = self.buf.as_init().as_ptr() as usize;
        let len = self.buf.as_init().len();
        let offset = self.offset;
        kqueue_syscall_blocking!({
            // Safety: the operation retains the owning file handle and buffer
            // until this blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let buf = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
            rustix::io::pwrite(fd, buf, offset)
                .map(|n| n as u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        BufResult(res, buf)
    }
}

#[cfg(windows)]
unsafe impl<B: IoBuf> IocpOperation for WriteAt<B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        let ptr = self.buf.as_init().as_ptr();
        let len = self.buf.as_init().len() as u32;
        let handle = self.io_handle.raw_handle();

        let mut interest = Interest::new(handle as _);

        unsafe {
            let overlapped = &mut *interest.as_mut_ptr();
            overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        let mut bytes_written = 0u32;
        windows_syscall_submit_overlapped!(interest, file, {
            WriteFile(
                handle as _,
                ptr as *const u8,
                len,
                &mut bytes_written,
                interest.as_mut_ptr(),
            )
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        BufResult(res, buf)
    }
}
