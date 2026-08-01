use std::{io, net::SocketAddr};

#[cfg(target_os = "linux")]
use std::io::IoSliceMut;

use socket2::SockAddr;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(target_os = "macos")]
use crate::driver::backends::kqueue::{PollAttempt, PollOperation};

use crate::{
    buf::{BoundedIoBufMut, BufResult},
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
pub(crate) struct RecvFrom<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
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
}

impl<B: BoundedIoBufMut> Op<RecvFrom<B>> {
    #![allow(unused_mut)] // The linux code uses mutablity
    pub(crate) fn recv_from(io_handle: &SharedIoHandle<socket2::Socket>, mut buf: B) -> io::Result<Op<RecvFrom<B>>> {
        let socket_addr = Box::new(unsafe { SockAddr::try_init(|_, _| Ok(()))?.1 });

        #[cfg(target_os = "linux")]
        let (io_slices, msghdr) = {
            let mut io_slices = vec![IoSliceMut::new(unsafe {
                std::slice::from_raw_parts_mut(buf.stable_write_ptr(), buf.bytes_total())
            })];

            let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msghdr.msg_iov = io_slices.as_mut_ptr().cast();
            msghdr.msg_iovlen = io_slices.len() as _;
            msghdr.msg_name = socket_addr.as_ptr() as *mut libc::c_void;
            msghdr.msg_namelen = socket_addr.len();

            (io_slices, msghdr)
        };

        #[cfg(windows)]
        let wsa_buf = windows_sys::Win32::Networking::WinSock::WSABUF {
            len: buf.bytes_total() as u32,
            buf: buf.stable_write_ptr() as *mut u8,
        };

        let data = RecvFrom {
            io_handle: io_handle.clone(),
            socket_addr,
            buf,
            #[cfg(target_os = "linux")]
            io_slices,
            #[cfg(target_os = "linux")]
            msghdr,
            #[cfg(windows)]
            wsa_buf: Box::new(wsa_buf),
            #[cfg(windows)]
            socket_addr_len: Box::new(0),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: BoundedIoBufMut> UringOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut() as *mut _).build()
    }

    // `mut self` is required on Windows (`set_length`); unused on other targets.
    #[allow(unused_mut)]
    fn complete(mut self, completion_result: Completion) -> Self::Output {
        let res = completion_result.result.map(|v| v as usize);
        let mut buf = self.buf;

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        buf,
                    );
                }
            };
            // Sync the address length that the kernel wrote through the
            // stable `lpFromlen` pointer during the overlapped operation.
            unsafe {
                self.socket_addr.set_length(address_len as _);
            }
        }

        let res = res.and_then(|bytes_written| {
            let socket_addr: SocketAddr = self.socket_addr.as_socket().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "kernel returned an invalid socket address")
            })?;

            // The kernel wrote `bytes_written` bytes to the buffer.
            unsafe {
                buf.set_init(bytes_written);
            }

            Ok((bytes_written, socket_addr))
        });

        (res, buf)
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> PollOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;
    fn attempt(&mut self) -> PollAttempt {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            let ptr = self.buf.stable_write_ptr();
            let len = self.buf.bytes_total();
            let mut addrlen = self.socket_addr.len();

            let result = macos_syscall!(libc::recvfrom(
                self.io_handle.raw_fd(),
                ptr as *mut libc::c_void,
                len,
                0,
                self.socket_addr.as_ptr() as *mut libc::sockaddr,
                &mut addrlen,
            ));

            if result.is_ok() {
                // Safety: the kernel wrote `addrlen` bytes of valid address data.
                unsafe {
                    self.socket_addr.set_length(addrlen);
                }
            }

            result
        })
    }

    // `mut self` is required on Windows (`set_length`); unused on other targets.
    #[allow(unused_mut)]
    fn complete(mut self, completion_result: Completion) -> Self::Output {
        let res = completion_result.result.map(|v| v as usize);
        let mut buf = self.buf;

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        buf,
                    );
                }
            };
            // Sync the address length that the kernel wrote through the
            // stable `lpFromlen` pointer during the overlapped operation.
            unsafe {
                self.socket_addr.set_length(address_len as _);
            }
        }

        let res = res.and_then(|bytes_written| {
            let socket_addr: SocketAddr = self.socket_addr.as_socket().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "kernel returned an invalid socket address")
            })?;

            // The kernel wrote `bytes_written` bytes to the buffer.
            unsafe {
                buf.set_init(bytes_written);
            }

            Ok((bytes_written, socket_addr))
        });

        (res, buf)
    }
}

#[cfg(windows)]
unsafe impl<B: BoundedIoBufMut> IocpOperation for RecvFrom<B> {
    type Output = BufResult<(usize, SocketAddr), B>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecvFrom;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;

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

    // `mut self` is required on Windows (`set_length`); unused on other targets.
    #[allow(unused_mut)]
    fn complete(mut self, completion_result: Completion) -> Self::Output {
        let res = completion_result.result.map(|v| v as usize);
        let mut buf = self.buf;

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        buf,
                    );
                }
            };
            // Sync the address length that the kernel wrote through the
            // stable `lpFromlen` pointer during the overlapped operation.
            unsafe {
                self.socket_addr.set_length(address_len as _);
            }
        }

        let res = res.and_then(|bytes_written| {
            let socket_addr: SocketAddr = self.socket_addr.as_socket().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "kernel returned an invalid socket address")
            })?;

            // The kernel wrote `bytes_written` bytes to the buffer.
            unsafe {
                buf.set_init(bytes_written);
            }

            Ok((bytes_written, socket_addr))
        });

        (res, buf)
    }
}
