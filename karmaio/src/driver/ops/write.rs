//! Offset-less write operations for stream-like file descriptors (pipes,
//! sockets, char devices). See [`crate::driver::ops::read`] for why the
//! offset-based [`crate::driver::ops::write_at`] ops are unsuitable for pipes.

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
    buf::{BoundedIoBuf, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Write<T, B> {
    // Holds a strong ref to the fd, preventing the pipe from being closed while
    // an operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle<T>,

    pub(crate) buf: B,
}

#[cfg(unix)]
impl<T, B> Op<Write<T, B>>
where
    T: AsRawFd + 'static,
    B: BoundedIoBuf + 'static,
{
    pub(crate) fn write(io_handle: &SharedIoHandle<T>, buf: B) -> io::Result<Op<Write<T, B>>> {
        let data = Write {
            io_handle: io_handle.clone(),
            buf,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime context").submit_op(data))
    }
}

#[cfg(windows)]
impl<T, B> Op<Write<T, B>>
where
    T: AsRawHandle + 'static,
    B: BoundedIoBuf + 'static,
{
    pub(crate) fn write(io_handle: &SharedIoHandle<T>, buf: B) -> io::Result<Op<Write<T, B>>> {
        let data = Write {
            io_handle: io_handle.clone(),
            buf,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<T: AsRawFd + 'static, B: BoundedIoBuf + 'static> UringOperation for Write<T, B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init();
        opcode::Write::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => (Ok(res as usize), self.buf),
            Err(err) => (Err(err), self.buf),
        }
    }
}

#[cfg(target_os = "macos")]
impl<T: AsRawFd + 'static, B: BoundedIoBuf + 'static> PollOperation for Write<T, B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> PollAttempt {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_WRITE, {
            let ptr = self.buf.stable_read_ptr() as *const libc::c_void;
            let len = self.buf.bytes_init();
            macos_syscall!(libc::write(self.io_handle.raw_fd(), ptr, len))
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => (Ok(res as usize), self.buf),
            Err(err) => (Err(err), self.buf),
        }
    }
}

#[cfg(windows)]
unsafe impl<T: AsRawHandle + 'static, B: BoundedIoBuf + 'static> IocpOperation for Write<T, B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        let ptr = self.buf.stable_read_ptr() as *const u8;
        let len = self.buf.bytes_init() as u32;
        let handle = self.io_handle.raw_handle();

        let mut interest = Interest::new(handle as _);
        windows_syscall_submit_overlapped!(interest, file, {
            WriteFile(handle as _, ptr, len, std::ptr::null_mut(), interest.as_mut_ptr())
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        match completion.result {
            Ok(res) => (Ok(res as usize), self.buf),
            Err(err) => (Err(err), self.buf),
        }
    }
}
