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
    buf::{BoundedIoBuf, BufResult},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Send<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: BoundedIoBuf> Op<Send<B>> {
    pub(crate) fn send(io_handle: &SharedIoHandle<socket2::Socket>, buf: B) -> std::io::Result<Op<Send<B>>> {
        #[cfg(windows)]
        let wsa_buf = windows_sys::Win32::Networking::WinSock::WSABUF {
            len: buf.bytes_init() as u32,
            buf: buf.stable_read_ptr() as *mut u8,
        };

        let data = Send {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf: Box::new(wsa_buf),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: BoundedIoBuf> UringOperation for Send<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init();

        opcode::Send::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<B: BoundedIoBuf> KqueueOperation for Send<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Write,
            {
                let ptr = self.buf.stable_read_ptr();
                let len = self.buf.bytes_init();

                kqueue_syscall!(libc::send(self.io_handle.raw_fd(), ptr as *const libc::c_void, len, 0,))
            }
        )
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}

#[cfg(windows)]
unsafe impl<B: BoundedIoBuf> IocpOperation for Send<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASend;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSASend(
                socket as _,
                self.wsa_buf.as_mut(),
                1,
                &mut bytes_sent,
                0,
                interest.as_mut_ptr(),
                None,
            )
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}
