use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use std::io::IoSlice;

use socket2::SockAddr;

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

// Implementation Notes -
//
// On Linux, iouring does not support sendto yet, so have to simulate it with sendmsg
// On Windows and macOS, the standard sendto syscalls are used
pub(crate) struct SendTo<B: IoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Held so the sockaddr remains valid for the kernel while the op is in-flight.
    #[allow(dead_code)]
    socket_addr: Box<SockAddr>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Held so the iovec memory stays valid for the kernel while the op is in-flight.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    io_slices: Vec<IoSlice<'static>>,

    // Pointer to the msghdr struct sent to the kernel
    #[cfg(target_os = "linux")]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: IoBuf> Op<SendTo<B>> {
    pub(crate) fn send_to(
        io_handle: &SharedIoHandle<socket2::Socket>,
        buf: B,
        socket_addr: SocketAddr,
    ) -> std::io::Result<Op<SendTo<B>>> {
        let socket_addr = Box::new(SockAddr::from(socket_addr));

        let data = SendTo {
            io_handle: io_handle.clone(),
            buf,
            socket_addr,
            #[cfg(target_os = "linux")]
            io_slices: Vec::new(),
            #[cfg(target_os = "linux")]
            msghdr: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(windows)]
            wsa_buf: Box::new(unsafe { std::mem::zeroed() }),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: IoBuf> UringOperation for SendTo<B> {
    type Output = BufResult<usize, B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let buf = self.buf.as_init();
        self.io_slices.clear();
        self.io_slices.push(IoSlice::new(unsafe {
            std::slice::from_raw_parts(buf.as_ptr(), buf.len())
        }));
        self.msghdr.msg_iov = self.io_slices.as_mut_ptr().cast();
        self.msghdr.msg_iovlen = self.io_slices.len() as _;
        self.msghdr.msg_name = self.socket_addr.as_ptr() as *mut _;
        self.msghdr.msg_namelen = self.socket_addr.len();

        opcode::SendMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_ref() as *const _).build()
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
impl<B: IoBuf> KqueueOperation for SendTo<B> {
    type Output = BufResult<usize, B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Write,
            {
                // Safety: `SockAddr` owns valid sockaddr storage for its
                // reported length, and the address is borrowed only for this
                // syscall.
                let address = unsafe {
                    rustix::net::SocketAddrAny::read(
                        self.socket_addr.as_ptr().cast::<rustix::net::addr::SocketAddrStorage>(),
                        self.socket_addr.len() as _,
                    )
                };
                // Safety: the operation owns the buffer for the duration of
                // the syscall, and the slice covers exactly its initialized
                // bytes.
                let buf = unsafe { std::slice::from_raw_parts(self.buf.as_init().as_ptr(), self.buf.as_init().len()) };
                rustix::net::sendto(&self.io_handle, buf, rustix::net::SendFlags::empty(), &address)
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
unsafe impl<B: IoBuf> IocpOperation for SendTo<B> {
    type Output = BufResult<usize, B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASendTo;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;
        self.wsa_buf.len = self.buf.as_init().len() as u32;
        self.wsa_buf.buf = self.buf.as_init().as_ptr() as *mut u8;

        let name = self.socket_addr.as_ptr() as *const _;
        let namelen = self.socket_addr.len() as i32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSASendTo(
                socket as _,
                self.wsa_buf.as_mut(),
                1,
                &mut bytes_sent,
                0,
                name,
                namelen,
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
