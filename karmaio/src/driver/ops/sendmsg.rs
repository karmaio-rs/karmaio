use std::net::SocketAddr;

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
    buf::{BufResult, IoBuf, IoVectoredBuf},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct SendMsg<V: IoVectoredBuf, C: IoBuf> {
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
    socket_addr: Option<Box<SockAddr>>,
    pub(crate) bufs: V,
    pub(crate) control: Option<C>,
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,
    #[cfg(unix)]
    msghdr: Box<libc::msghdr>,
    #[cfg(windows)]
    wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<V: IoVectoredBuf, C: IoBuf> Op<SendMsg<V, C>> {
    pub(crate) fn sendmsg(
        io_handle: &SharedIoHandle<socket2::Socket>,
        bufs: V,
        control: Option<C>,
        socket_addr: Option<SocketAddr>,
    ) -> std::io::Result<Op<SendMsg<V, C>>> {
        let data = SendMsg {
            io_handle: io_handle.clone(),
            socket_addr: socket_addr.map(|addr| Box::new(SockAddr::from(addr))),
            bufs,
            control,
            #[cfg(unix)]
            iovs: Vec::new(),
            #[cfg(unix)]
            msghdr: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(windows)]
            wsa_bufs: Vec::new(),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<V: IoVectoredBuf, C: IoBuf> SendMsg<V, C> {
    #[cfg(unix)]
    fn rebuild_message(&mut self) {
        self.iovs = self
            .bufs
            .iter_slice()
            .map(|buf| libc::iovec {
                iov_base: buf.as_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            })
            .collect();
        self.msghdr.msg_iov = self.iovs.as_mut_ptr();
        self.msghdr.msg_iovlen = self.iovs.len() as _;

        if let Some(addr) = &self.socket_addr {
            self.msghdr.msg_name = addr.as_ptr() as *mut _;
            self.msghdr.msg_namelen = addr.len();
        } else {
            self.msghdr.msg_name = std::ptr::null_mut();
            self.msghdr.msg_namelen = 0;
        }

        if let Some(control) = &self.control {
            self.msghdr.msg_control = control.as_init().as_ptr() as *mut _;
            self.msghdr.msg_controllen = control.as_init().len() as _;
        } else {
            self.msghdr.msg_control = std::ptr::null_mut();
            self.msghdr.msg_controllen = 0;
        }
    }

    #[cfg(windows)]
    fn rebuild_wsa_bufs(&mut self) {
        self.wsa_bufs = self
            .bufs
            .iter_slice()
            .map(|buf| windows_sys::Win32::Networking::WinSock::WSABUF {
                len: buf.len() as u32,
                buf: buf.as_ptr() as *mut u8,
            })
            .collect();
    }

    fn finish(self, completion: super::Completion) -> BufResult<(usize, Option<C>), V> {
        let result = completion.result.map(|n| (n as usize, self.control));
        BufResult(result, self.bufs)
    }
}

#[cfg(target_os = "linux")]
unsafe impl<V: IoVectoredBuf, C: IoBuf> UringOperation for SendMsg<V, C> {
    type Output = BufResult<(usize, Option<C>), V>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        self.rebuild_message();
        opcode::SendMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_ref()).build()
    }

    fn complete(self, completion: super::Completion) -> Self::Output {
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
impl<V: IoVectoredBuf, C: IoBuf> KqueueOperation for SendMsg<V, C> {
    type Output = BufResult<(usize, Option<C>), V>;

    fn attempt(&mut self) -> KqueueAttempt {
        self.rebuild_message();
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Write,
            { kqueue_syscall!(libc::sendmsg(self.io_handle.raw_fd(), self.msghdr.as_ref(), 0,)) }
        )
    }

    fn complete(self, completion: super::Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(windows)]
unsafe impl<V: IoVectoredBuf, C: IoBuf> IocpOperation for SendMsg<V, C> {
    type Output = BufResult<(usize, Option<C>), V>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASendTo;

        self.rebuild_wsa_bufs();
        let socket = self.io_handle.raw_socket();
        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;
        let (name, namelen) = self.socket_addr.as_ref().map_or((std::ptr::null(), 0), |addr| {
            (addr.as_ptr() as *const _, addr.len() as i32)
        });

        windows_syscall_submit_overlapped!(interest, socket, {
            WSASendTo(
                socket as _,
                self.wsa_bufs.as_mut_ptr(),
                self.wsa_bufs.len() as u32,
                &mut bytes_sent,
                0,
                name,
                namelen,
                interest.as_mut_ptr(),
                None,
            )
        })
    }

    fn complete(self, completion: super::Completion) -> Self::Output {
        self.finish(completion)
    }
}
