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
    buf::{BoundedIoBufMut, BufResult},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Recv<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: BoundedIoBufMut> Op<Recv<B>> {
    // `mut buf` is required on Windows (`stable_write_ptr`); unused on other targets.
    #[allow(unused_mut)]
    pub(crate) fn recv(io_handle: &SharedIoHandle<socket2::Socket>, mut buf: B) -> std::io::Result<Op<Recv<B>>> {
        #[cfg(windows)]
        let wsa_buf = windows_sys::Win32::Networking::WinSock::WSABUF {
            len: buf.bytes_total() as u32,
            buf: buf.stable_write_ptr() as *mut u8,
        };

        let data = Recv {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf: Box::new(wsa_buf),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: BoundedIoBufMut> UringOperation for Recv<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total();

        opcode::Recv::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut buf = self.buf;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            // Safety: the kernel wrote `n` bytes to the buffer.
            unsafe {
                buf.set_init(n);
            }
        }

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
impl<B: BoundedIoBufMut> KqueueOperation for Recv<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            {
                // Safety: the operation owns this buffer for the duration of the
                // syscall, and the slice covers exactly its writable capacity.
                let buf =
                    unsafe { std::slice::from_raw_parts_mut(self.buf.stable_write_ptr(), self.buf.bytes_total()) };
                rustix::net::recv(&self.io_handle, buf, rustix::net::RecvFlags::empty())
                    .map(|(_, n)| n as u32)
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut buf = self.buf;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            // Safety: the kernel wrote `n` bytes to the buffer.
            unsafe {
                buf.set_init(n);
            }
        }

        (res, buf)
    }
}

#[cfg(windows)]
unsafe impl<B: BoundedIoBufMut> IocpOperation for Recv<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecv;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSARecv(
                socket as _,
                self.wsa_buf.as_mut(),
                1,
                &mut bytes_recv,
                &mut flags,
                interest.as_mut_ptr(),
                None,
            )
        })
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut buf = self.buf;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            // Safety: the kernel wrote `n` bytes to the buffer.
            unsafe {
                buf.set_init(n);
            }
        }

        (res, buf)
    }
}
