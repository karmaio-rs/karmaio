use std::{io::IoSliceMut, net::SocketAddr};

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
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct RecvMsg<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    pub(crate) socket_addr: Box<SockAddr>,

    // Reference to the in-flight buffers.
    pub(crate) bufs: Vec<B>,

    // Internal pointers to the IOVEC/WSABUF structs.
    #[allow(dead_code)] // This to ensure that the pointers are valid
    io_slices: Vec<IoSliceMut<'static>>,

    // Pointer to the msghdr struct sent to the kernel (POSIX only).
    #[cfg(unix)]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF>,

    // Stable memory for `lpFromlen` passed to WSARecvFrom (Windows only).
    #[cfg(windows)]
    socket_addr_len: Box<i32>,
}

impl<B: BoundedIoBufMut> Op<RecvMsg<B>> {
    pub(crate) fn recvmsg(
        io_handle: &SharedIoHandle<socket2::Socket>,
        mut bufs: Vec<B>,
    ) -> std::io::Result<Op<RecvMsg<B>>> {
        let mut io_slices = Vec::with_capacity(bufs.len());
        for buf in &mut bufs {
            io_slices.push(IoSliceMut::new(unsafe {
                std::slice::from_raw_parts_mut(buf.stable_write_ptr(), buf.bytes_total())
            }));
        }

        let socket_addr = Box::new(unsafe { SockAddr::try_init(|_, _| Ok(()))?.1 });

        #[cfg(unix)]
        let msghdr = {
            let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msghdr.msg_iov = io_slices.as_mut_ptr().cast();
            msghdr.msg_iovlen = io_slices.len() as _;
            msghdr.msg_name = socket_addr.as_ptr() as *mut libc::c_void;
            msghdr.msg_namelen = socket_addr.len();
            msghdr
        };

        #[cfg(windows)]
        let wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF> = bufs
            .iter_mut()
            .map(|buf| windows_sys::Win32::Networking::WinSock::WSABUF {
                len: buf.bytes_total() as u32,
                buf: buf.stable_write_ptr() as *mut u8,
            })
            .collect();

        let data = RecvMsg {
            io_handle: io_handle.clone(),
            socket_addr,
            io_slices,
            bufs,
            #[cfg(unix)]
            msghdr,
            #[cfg(windows)]
            wsa_bufs,
            #[cfg(windows)]
            socket_addr_len: Box::new(0),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl<B: BoundedIoBufMut> UringOperation for RecvMsg<B> {
    type Output = BufResult<(usize, SocketAddr), Vec<B>>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

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
impl<B: BoundedIoBufMut> KqueueOperation for RecvMsg<B> {
    type Output = BufResult<(usize, SocketAddr), Vec<B>>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            {
                kqueue_syscall!(libc::recvmsg(
                    self.io_handle.raw_fd(),
                    self.msghdr.as_mut() as *mut libc::msghdr,
                    0,
                ))
            }
        )
    }

    fn complete(self, completion_result: Completion) -> Self::Output {
        self.finish(completion_result)
    }
}

#[cfg(windows)]
unsafe impl<B: BoundedIoBufMut> IocpOperation for RecvMsg<B> {
    type Output = BufResult<(usize, SocketAddr), Vec<B>>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecvFrom;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_recv = 0u32;
        let mut flags = 0u32;

        *self.socket_addr_len = self.socket_addr.len() as i32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSARecvFrom(
                socket as _,
                self.wsa_bufs.as_ptr() as *const _,
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

    fn complete(self, completion_result: Completion) -> Self::Output {
        self.finish(completion_result)
    }
}

impl<B: BoundedIoBufMut> RecvMsg<B> {
    fn finish(mut self, completion_result: Completion) -> BufResult<(usize, SocketAddr), Vec<B>> {
        let capacity: usize = self.bufs.iter().map(|buf| buf.bytes_total()).sum();
        let total_bytes_written = match completion_result.bytes_transferred(capacity) {
            Ok(n) => n,
            Err(err) => return (Err(err), self.bufs),
        };

        #[cfg(windows)]
        {
            let address_len = match usize::try_from(*self.socket_addr_len) {
                Ok(length) if length <= self.socket_addr.len() as usize => length,
                _ => {
                    return (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid socket address length",
                        )),
                        self.bufs,
                    );
                }
            };
            unsafe {
                self.socket_addr.set_length(address_len as _);
            }
        }

        let socket_addr = match self.socket_addr.as_socket() {
            Some(addr) => addr,
            None => {
                return (
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "kernel returned an invalid socket address",
                    )),
                    self.bufs,
                );
            }
        };

        // The kernel fills buffers to capacity one after another.
        let mut remaining = total_bytes_written;
        for buf in &mut self.bufs {
            let bytes_written = std::cmp::min(remaining, buf.bytes_total());
            // Safety: `bytes_transferred` capped the total to the sum of capacities.
            unsafe {
                buf.set_init(bytes_written);
            }
            remaining -= bytes_written;
            if remaining == 0 {
                break;
            }
        }
        debug_assert_eq!(remaining, 0);

        (Ok((total_bytes_written, socket_addr)), self.bufs)
    }
}
