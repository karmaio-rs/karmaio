use std::{io, net::SocketAddr};

#[cfg(target_os = "linux")]
use std::io::IoSliceMut;

#[cfg(any(target_os = "linux", windows))]
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
    buf::{BufResult, IoBufMut},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

// Implementation Notes -
//
// On Linux, iouring does not support recvfrom yet, so have to simulate it with recvmsg
// On Windows and macOS, the standard recvfrom syscalls are used
pub(crate) struct RecvFrom<B: IoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) socket_addr: Box<SockAddr>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Held so the iovec memory stays valid for the kernel while the op is in-flight.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    io_slices: Vec<IoSliceMut<'static>>,

    // Pointer to the msghdr struct sent to the kernel
    #[cfg(target_os = "linux")]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,

    // Stable memory for the address length pointer passed to WSARecvFrom.
    // The kernel writes to this asynchronously via the overlapped I/O path.
    #[cfg(windows)]
    socket_addr_len: Box<i32>,

    // Address returned by the synchronous Kqueue recvfrom attempt.
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    received_address: Option<rustix::net::SocketAddrAny>,
}

impl<B: IoBufMut> Op<RecvFrom<B>> {
    // On failure the buffer is returned with the error; the kernel never
    // observed the operation.
    pub(crate) fn recv_from(
        io_handle: &SharedIoHandle<socket2::Socket>,
        buf: B,
    ) -> std::result::Result<Op<RecvFrom<B>>, (io::Error, B)> {
        #[cfg(any(target_os = "linux", windows))]
        // Infallible: the initializer closure always returns `Ok`.
        let socket_addr = Box::new(unsafe { SockAddr::try_init(|_, _| Ok(())).expect("sockaddr init cannot fail").1 });

        let data = RecvFrom {
            io_handle: io_handle.clone(),
            #[cfg(any(target_os = "linux", windows))]
            socket_addr,
            buf,
            #[cfg(target_os = "linux")]
            io_slices: Vec::new(),
            #[cfg(target_os = "linux")]
            msghdr: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(windows)]
            wsa_buf: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(windows)]
            socket_addr_len: Box::new(0),
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "dragonfly"
            ))]
            received_address: None,
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
unsafe impl<B: IoBufMut> UringOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let ptr = self.buf.as_uninit().as_mut_ptr().cast::<u8>();
        let len = self.buf.as_uninit().len();
        self.io_slices.clear();
        self.io_slices
            .push(IoSliceMut::new(unsafe { std::slice::from_raw_parts_mut(ptr, len) }));
        self.msghdr.msg_iov = self.io_slices.as_mut_ptr().cast();
        self.msghdr.msg_iovlen = self.io_slices.len() as _;
        self.msghdr.msg_name = self.socket_addr.as_ptr() as *mut _;
        self.msghdr.msg_namelen = self.socket_addr.len();

        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut() as *mut _).build()
    }

    fn complete(self, completion_result: Completion) -> Self::Output {
        self.finish(completion_result)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<B: IoBufMut> KqueueOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            {
                // Safety: the operation owns the buffer for the duration of
                // the syscall, and the slice covers exactly its writable
                // capacity.
                let buf = unsafe {
                    std::slice::from_raw_parts_mut(
                        self.buf.as_uninit().as_mut_ptr().cast::<u8>(),
                        self.buf.as_uninit().len(),
                    )
                };
                rustix::net::recvfrom(&self.io_handle, buf, rustix::net::RecvFlags::empty())
                    .map(|(_, n, address)| {
                        self.received_address = address;
                        n as u32
                    })
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(self, completion_result: Completion) -> Self::Output {
        self.finish(completion_result)
    }
}

#[cfg(windows)]
unsafe impl<B: IoBufMut> IocpOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecvFrom;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;
        self.wsa_buf.len = self.buf.as_uninit().len() as u32;
        self.wsa_buf.buf = self.buf.as_uninit().as_mut_ptr().cast::<u8>();

        // Must reside in stable memory for the overlapped I/O path.
        *self.socket_addr_len = self.socket_addr.len() as i32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSARecvFrom(
                socket as _,
                self.wsa_buf.as_mut(),
                1,
                &mut bytes_recv,
                &mut flags,
                self.socket_addr.as_ptr() as *mut _,
                self.socket_addr_len.as_mut(),
                interest.as_mut_ptr(),
                None,
            )
        })
    }

    fn complete(self, completion_result: Completion) -> Self::Output {
        self.finish(completion_result)
    }
}

impl<B: IoBufMut> RecvFrom<B> {
    fn finish(mut self, completion_result: Completion) -> BufResult<(usize, SocketAddr), B> {
        let capacity = self.buf.as_uninit().len();
        let bytes_written = match completion_result.bytes_transferred(capacity) {
            Ok(n) => n,
            Err(err) => return BufResult(Err(err), self.buf),
        };

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return BufResult(
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        self.buf,
                    );
                }
            };
            // Sync the address length that the kernel wrote through the
            // stable `lpFromlen` pointer during the overlapped operation.
            unsafe {
                self.socket_addr.set_length(address_len as _);
            }
        }

        let socket_addr = {
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "dragonfly"
            ))]
            {
                let address = match self.received_address.take() {
                    Some(address) => address,
                    None => {
                        return BufResult(
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "kernel returned no socket address",
                            )),
                            self.buf,
                        );
                    }
                };
                match SocketAddr::try_from(address) {
                    Ok(addr) => addr,
                    Err(err) => return BufResult(Err(io::Error::from(err)), self.buf),
                }
            }
            #[cfg(any(target_os = "linux", windows))]
            {
                match self.socket_addr.as_socket() {
                    Some(addr) => addr,
                    None => {
                        return BufResult(
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "kernel returned an invalid socket address",
                            )),
                            self.buf,
                        );
                    }
                }
            }
        };

        // Safety: the platform wrote at most `capacity` bytes into the buffer.
        unsafe {
            self.buf.set_len(bytes_written);
        }

        BufResult(Ok((bytes_written, socket_addr)), self.buf)
    }
}
