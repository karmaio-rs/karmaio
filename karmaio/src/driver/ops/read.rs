//! Offset-less read operations for stream-like file descriptors (pipes, sockets,
//! char devices). Unlike [`crate::driver::ops::read_at`], these never touch the
//! offset, which matters on macOS/BSD where the kqueue `Submittable`
//! implementations of the offset-based ops use `pread`/`pwrite` and those
//! syscalls fail (`ESPIPE`) on non-seekable descriptors.

use std::io;

use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::{OsRawHandle, SharedIoHandle},
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Read<B> {
    // Holds a strong ref to the fd, preventing the pipe from being closed while
    // an operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle,

    pub(crate) buf: B,
}

impl<B: BoundedIoBufMut> Op<Read<B>> {
    pub(crate) fn read(io_handle: &SharedIoHandle, buf: B) -> io::Result<Op<Read<B>>> {
        let data = Read {
            io_handle: io_handle.clone(),
            buf,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBufMut> Operable for Read<B> {}

impl<B: BoundedIoBufMut> Submittable for Read<B> {
    #[cfg(target_os = "linux")]
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total();
        opcode::Read::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    #[cfg(target_os = "macos")]
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            let ptr = self.buf.stable_write_ptr() as *mut libc::c_void;
            let len = self.buf.bytes_total();
            macos_syscall!(libc::read(self.io_handle.raw_fd(), ptr, len))
        })
    }

    #[cfg(windows)]
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let ptr = self.buf.stable_write_ptr() as *mut u8;
        let len = self.buf.bytes_total() as u32;

        match self.io_handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => {
                let mut interest = Interest::new(handle as _);
                windows_syscall_submit_overlapped!(interest, file, {
                    ReadFile(handle as _, ptr, len, std::ptr::null_mut(), interest.as_mut_ptr())
                })
            }
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "use recv for socket reads on Windows",
                )),
                flags: 0,
            }),
        }
    }
}

impl<B: BoundedIoBufMut> Completable for Read<B> {
    type Result = BufResult<usize, B>;

    fn complete(mut self, completion: Completion) -> Self::Result {
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

/// Reads from a raw [`OsRawHandle`] synchronously. Used to drain a child's piped
/// output on the blocking pool, where the `Rc`-backed [`SharedIoHandle`] cannot
/// cross thread boundaries.
pub(crate) fn read_sync_raw(fd: usize, buf: &mut [u8]) -> io::Result<usize> {
    let len = buf.len();
    #[cfg(unix)]
    {
        macos_syscall!(libc::read(
            fd as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            len
        ))
        .map(|n| n as usize)
    }
    #[cfg(windows)]
    {
        let mut n = 0u32;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                fd as windows_sys::Win32::Foundation::HANDLE,
                buf.as_mut_ptr(),
                len as _,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            Ok(n as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
