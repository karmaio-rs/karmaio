//! Offset-less read operations for stream-like file descriptors (pipes, sockets,
//! char devices). Unlike [`crate::driver::ops::read_at`], these never touch the
//! offset, which matters on macOS/BSD where the kqueue `PollOperation`
//! implementations of the offset-based ops use `pread`/`pwrite` and those
//! syscalls fail (`ESPIPE`) on non-seekable descriptors.

use std::io;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(target_os = "macos")]
use crate::driver::backends::kqueue::{PollAttempt, PollOperation};

use crate::{
    buf::{BoundedIoBufMut, BufResult},
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
    T: AsRawFd + 'static,
    B: BoundedIoBufMut + 'static,
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
    B: BoundedIoBufMut + 'static,
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
unsafe impl<T: AsRawFd + 'static, B: BoundedIoBufMut + 'static> UringOperation for Read<T, B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total();
        opcode::Read::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => {
                let res = res as usize;
                // Safety: the kernel wrote `res` bytes into the buffer.
                unsafe {
                    self.buf.set_init(res);
                }
                (Ok(res), self.buf)
            }
            Err(err) => (Err(err), self.buf),
        }
    }
}

#[cfg(target_os = "macos")]
impl<T: AsRawFd + 'static, B: BoundedIoBufMut + 'static> PollOperation for Read<T, B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> PollAttempt {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            let ptr = self.buf.stable_write_ptr() as *mut libc::c_void;
            let len = self.buf.bytes_total();
            macos_syscall!(libc::read(self.io_handle.raw_fd(), ptr, len))
        })
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => {
                let res = res as usize;
                // Safety: the kernel wrote `res` bytes into the buffer.
                unsafe {
                    self.buf.set_init(res);
                }
                (Ok(res), self.buf)
            }
            Err(err) => (Err(err), self.buf),
        }
    }
}

#[cfg(windows)]
unsafe impl<T: AsRawHandle + 'static, B: BoundedIoBufMut + 'static> IocpOperation for Read<T, B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let ptr = self.buf.stable_write_ptr() as *mut u8;
        let len = self.buf.bytes_total() as u32;
        let handle = self.io_handle.raw_handle();

        let mut interest = Interest::new(handle as _);
        windows_syscall_submit_overlapped!(interest, file, {
            ReadFile(handle as _, ptr, len, std::ptr::null_mut(), interest.as_mut_ptr())
        })
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => {
                let res = res as usize;
                // Safety: the kernel wrote `res` bytes into the buffer.
                unsafe {
                    self.buf.set_init(res);
                }
                (Ok(res), self.buf)
            }
            Err(err) => (Err(err), self.buf),
        }
    }
}
