//! Offset-less read operations for stream-like file descriptors (pipes, sockets,
//! char devices). Unlike [`crate::driver::ops::read_at`], these never touch the
//! offset, which matters on macOS/BSD where the kqueue `KqueueOperation`
//! implementations of the offset-based ops use `pread`/`pwrite` and those
//! syscalls fail (`ESPIPE`) on non-seekable descriptors.

use std::io;

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

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
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Read<T, B> {
    // Holds a strong ref to the fd, preventing the pipe from being closed while
    // an operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle<T>,

    pub(crate) buf: B,
}

#[cfg(unix)]
impl<T, B> Op<Read<T, B>>
where
    T: AsFd + AsRawFd + 'static,
    B: IoBufMut + 'static,
{
    pub(crate) fn read(io_handle: &SharedIoHandle<T>, buf: B) -> io::Result<Op<Read<T, B>>> {
        let data = Read {
            io_handle: io_handle.clone(),
            buf,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime context").submit_op(data))
    }
}

#[cfg(windows)]
impl<T, B> Op<Read<T, B>>
where
    T: AsRawHandle + 'static,
    B: IoBufMut + 'static,
{
    pub(crate) fn read(io_handle: &SharedIoHandle<T>, buf: B) -> io::Result<Op<Read<T, B>>> {
        let data = Read {
            io_handle: io_handle.clone(),
            buf,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<T: AsRawFd + 'static, B: IoBufMut + 'static> UringOperation for Read<T, B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>();
        let len = self.buf.as_uninit().len();
        opcode::Read::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
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
impl<T: AsFd + AsRawFd + 'static, B: IoBufMut + 'static> KqueueOperation for Read<T, B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            {
                // Safety: the operation owns this buffer for the duration of the
                // syscall, and the slice covers exactly its writable capacity.
                let buf = unsafe {
                    std::slice::from_raw_parts_mut(
                        self.buf.as_uninit().as_mut_ptr().cast::<u8>(),
                        self.buf.as_uninit().len(),
                    )
                };
                rustix::io::read(&self.io_handle, buf)
                    .map(|n| n as u32)
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(windows)]
unsafe impl<T: AsRawHandle + 'static, B: IoBufMut + 'static> IocpOperation for Read<T, B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>() as *mut u8;
        let len = self.buf.as_uninit().len() as u32;
        let handle = self.io_handle.raw_handle();

        let mut interest = Interest::new(handle as _);
        windows_syscall_submit_overlapped!(interest, file, {
            ReadFile(handle as _, ptr, len, std::ptr::null_mut(), interest.as_mut_ptr())
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

impl<T, B: IoBufMut> Read<T, B> {
    fn finish(mut self, completion: Completion) -> BufResult<usize, B> {
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
