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

pub(crate) struct Recv<B: IoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: IoBufMut> Op<Recv<B>> {
    pub(crate) fn recv(io_handle: &SharedIoHandle<socket2::Socket>, buf: B) -> std::io::Result<Op<Recv<B>>> {
        let data = Recv {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: IoBufMut> UringOperation for Recv<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>();
        let len = self.buf.as_uninit().len();

        opcode::Recv::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
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
impl<B: IoBufMut> KqueueOperation for Recv<B> {
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
                rustix::net::recv(&self.io_handle, buf, rustix::net::RecvFlags::empty())
                    .map(|(_, n)| n as u32)
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(self, completion_entry: super::Completion) -> Self::Output {
        self.finish(completion_entry)
    }
}

#[cfg(windows)]
unsafe impl<B: IoBufMut> IocpOperation for Recv<B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecv;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;
        self.wsa_buf.len = self.buf.as_uninit().len() as u32;
        self.wsa_buf.buf = self.buf.as_uninit().as_mut_ptr().cast::<u8>();

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
        self.finish(completion_entry)
    }
}

impl<B: IoBufMut> Recv<B> {
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
