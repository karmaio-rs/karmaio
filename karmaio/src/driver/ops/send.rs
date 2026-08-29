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

pub(crate) struct Send<B: IoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: IoBuf> Op<Send<B>> {
    // On failure the buffer is returned with the error; the kernel never
    // observed the operation.
    pub(crate) fn send(
        io_handle: &SharedIoHandle<socket2::Socket>,
        buf: B,
    ) -> Result<Op<Send<B>>, (std::io::Error, B)> {
        let data = Send {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        CURRENT_DRIVER.with(|handle| {
            handle
                .upgrade()
                .expect("Not in a runtime context")
                .try_submit_op(data)
                .map_err(|(error, data)| (error, data.buf))
        })
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: IoBuf> UringOperation for Send<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.as_init().as_ptr();
        let len = self.buf.as_init().len();

        opcode::Send::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        let res = completion_entry.result.map(|v| v as usize);
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
impl<B: IoBuf> KqueueOperation for Send<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Write,
            {
                // Safety: the operation owns this buffer for the duration of the
                // syscall, and the slice covers exactly its initialized bytes.
                let buf = unsafe { std::slice::from_raw_parts(self.buf.as_init().as_ptr(), self.buf.as_init().len()) };
                rustix::net::send(&self.io_handle, buf, rustix::net::SendFlags::empty())
                    .map(|n| n as u32)
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        BufResult(res, buf)
    }
}

#[cfg(windows)]
unsafe impl<B: IoBuf> IocpOperation for Send<B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASend;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;
        self.wsa_buf.len = self.buf.as_init().len() as u32;
        self.wsa_buf.buf = self.buf.as_init().as_ptr() as *mut u8;

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

        BufResult(res, buf)
    }
}
