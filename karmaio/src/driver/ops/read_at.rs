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
    buf::{BufResult, IoBufMut},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct ReadAt<B: IoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle<std::fs::File>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Read offset
    offset: u64,
}

impl<B: IoBufMut> Op<ReadAt<B>> {
    pub(crate) fn read_at(
        io_handle: &SharedIoHandle<std::fs::File>,
        buf: B,
        offset: u64,
    ) -> std::io::Result<Op<ReadAt<B>>> {
        let data = ReadAt {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: IoBufMut> UringOperation for ReadAt<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>();
        let len = self.buf.as_uninit().len();
        opcode::Read::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _)
            .offset(self.offset as _)
            .build()
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        self.finish(completion_entry)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<B: IoBufMut> KqueueOperation for ReadAt<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        // Regular files always report ready under kqueue, and pread can block on
        // disk I/O — offload to the blocking pool so the runtime thread stays free.
        // Buffer pointers remain valid: the Op (and its buffers) stay alive until
        // the pool job completes and `complete` runs.
        let fd = self.io_handle.raw_fd();
        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>() as usize;
        let len = self.buf.as_uninit().len();
        let offset = self.offset;
        kqueue_syscall_blocking!({
            // Safety: the operation retains the owning file handle and buffer
            // until this blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
            rustix::io::pread(fd, buf, offset)
                .map(|n| n as u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        self.finish(completion_entry)
    }
}

#[cfg(windows)]
unsafe impl<B: IoBufMut> IocpOperation for ReadAt<B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>();
        let len = self.buf.as_uninit().len() as u32;
        let handle = self.io_handle.raw_handle();

        let mut interest = Interest::new(handle as _);

        unsafe {
            let overlapped = &mut *interest.as_mut_ptr();
            overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        let mut bytes_read = 0u32;
        windows_syscall_submit_overlapped!(interest, file, {
            ReadFile(handle as _, ptr as *mut u8, len, &mut bytes_read, interest.as_mut_ptr())
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        self.finish(completion_entry)
    }
}

impl<B: IoBufMut> ReadAt<B> {
    fn finish(mut self, completion: super::Completion) -> BufResult<usize, B> {
        let capacity = self.buf.as_uninit().len();
        match completion.bytes_transferred(capacity) {
            Ok(n) => {
                // Safety: the platform wrote at most `capacity` bytes into the buffer.
                unsafe {
                    self.buf.set_len(n);
                }
                BufResult(Ok(n), self.buf)
            }
            Err(err) => BufResult(Err(err), self.buf),
        }
    }
}
