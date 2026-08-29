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
    buf::{BufResult, IoVectoredBufMut},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct RecvMsg<V: IoVectoredBufMut> {
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
    socket_addr: Box<SockAddr>,
    pub(crate) bufs: V,
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,
    #[cfg(unix)]
    msghdr: Box<libc::msghdr>,
    #[cfg(windows)]
    wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF>,
    #[cfg(windows)]
    socket_addr_len: Box<i32>,
}

impl<V: IoVectoredBufMut> Op<RecvMsg<V>> {
    // On failure the buffers are returned with the error; the kernel never
    // observed the operation.
    pub(crate) fn recvmsg(
        io_handle: &SharedIoHandle<socket2::Socket>,
        bufs: V,
    ) -> Result<Op<RecvMsg<V>>, (std::io::Error, V)> {
        let data = RecvMsg {
            io_handle: io_handle.clone(),
            // Infallible: the initializer closure always returns `Ok`.
            socket_addr: Box::new(unsafe { SockAddr::try_init(|_, _| Ok(())).expect("sockaddr init cannot fail").1 }),
            bufs,
            #[cfg(unix)]
            iovs: Vec::new(),
            #[cfg(unix)]
            msghdr: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(windows)]
            wsa_bufs: Vec::new(),
            #[cfg(windows)]
            socket_addr_len: Box::new(0),
        };

        CURRENT_DRIVER.with(|handle| {
            handle
                .upgrade()
                .expect("Not in a runtime context")
                .try_submit_op(data)
                .map_err(|(error, data)| (error, data.bufs))
        })
    }
}

impl<V: IoVectoredBufMut> RecvMsg<V> {
    #[cfg(unix)]
    fn rebuild_message(&mut self) {
        self.iovs = self
            .bufs
            .iter_uninit_slice()
            .map(|buf| libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            })
            .collect();
        self.msghdr.msg_iov = self.iovs.as_mut_ptr();
        self.msghdr.msg_iovlen = self.iovs.len() as _;
        self.msghdr.msg_name = self.socket_addr.as_ptr() as *mut _;
        self.msghdr.msg_namelen = self.socket_addr.len();
    }

    #[cfg(windows)]
    fn rebuild_wsa_bufs(&mut self) {
        self.wsa_bufs = self
            .bufs
            .iter_uninit_slice()
            .map(|buf| windows_sys::Win32::Networking::WinSock::WSABUF {
                len: buf.len() as u32,
                buf: buf.as_mut_ptr().cast(),
            })
            .collect();
    }

    fn finish(mut self, completion: Completion) -> BufResult<(usize, SocketAddr), V> {
        let capacity = self.bufs.total_capacity();
        let written = match completion.bytes_transferred(capacity) {
            Ok(n) => n,
            Err(error) => return BufResult(Err(error), self.bufs),
        };

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return BufResult(
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        self.bufs,
                    );
                }
            };
            unsafe { self.socket_addr.set_length(address_len as _) };
        }

        let Some(socket_addr) = self.socket_addr.as_socket() else {
            return BufResult(
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "kernel returned an invalid socket address",
                )),
                self.bufs,
            );
        };

        // Safety: bytes_transferred verified that the aggregate initialized
        // prefix fits within the submitted capacity.
        unsafe { self.bufs.set_len(written) };
        BufResult(Ok((written, socket_addr)), self.bufs)
    }
}

#[cfg(target_os = "linux")]
unsafe impl<V: IoVectoredBufMut> UringOperation for RecvMsg<V> {
    type Output = BufResult<(usize, SocketAddr), V>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        self.rebuild_message();
        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut()).build()
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
impl<V: IoVectoredBufMut> KqueueOperation for RecvMsg<V> {
    type Output = BufResult<(usize, SocketAddr), V>;

    fn attempt(&mut self) -> KqueueAttempt {
        self.rebuild_message();
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            { kqueue_syscall!(libc::recvmsg(self.io_handle.raw_fd(), self.msghdr.as_mut(), 0,)) }
        )
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(windows)]
unsafe impl<V: IoVectoredBufMut> IocpOperation for RecvMsg<V> {
    type Output = BufResult<(usize, SocketAddr), V>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecvFrom;

        self.rebuild_wsa_bufs();
        let socket = self.io_handle.raw_socket();
        let mut interest = Interest::new(socket as _);
        let mut bytes_recv = 0u32;
        let mut flags = 0u32;
        *self.socket_addr_len = self.socket_addr.len() as i32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSARecvFrom(
                socket as _,
                self.wsa_bufs.as_ptr(),
                self.wsa_bufs.len() as u32,
                &mut bytes_recv,
                &mut flags,
                self.socket_addr.as_ptr() as *mut _,
                self.socket_addr_len.as_mut(),
                interest.as_mut_ptr(),
                None,
            )
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}
