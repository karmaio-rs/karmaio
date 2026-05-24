use std::{io, net::SocketAddr};

#[cfg(target_os = "linux")]
use std::io::IoSliceMut;

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
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
    io_handle: SharedIoHandle,
    pub(crate) socket_addr: Box<SockAddr>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Internal pointers to the IOVEC strcuts
    #[cfg(target_os = "linux")]
    io_slices: Vec<IoSliceMut<'static>>,

    // Pointer to the msghdr struct sent to the kernel
    #[cfg(target_os = "linux")]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: windows_sys::Win32::Networking::WinSock::WSABUF,

    // Stable memory for the address length pointer passed to WSARecvFrom.
    // The kernel writes to this asynchronously via the overlapped I/O path.
    #[cfg(windows)]
    socket_addr_len: i32,
}

impl<B: BoundedIoBufMut> Op<RecvFrom<B>> {
    #![allow(unused_mut)] // The linux code uses mutablity
    pub(crate) fn recv_from(io_handle: &SharedIoHandle, mut buf: B) -> io::Result<Op<RecvFrom<B>>> {
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
            msghdr.msg_namelen = socket_addr.len() as u32;

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
            wsa_buf,
            #[cfg(windows)]
            socket_addr_len: 0,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBufMut> Operable for RecvFrom<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBufMut> Submittable for RecvFrom<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut() as *mut _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> Submittable for RecvFrom<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::kqueue::Interest;

        loop {
            let ptr = self.buf.stable_write_ptr();
            let len = self.buf.bytes_total();
            let mut addrlen = self.socket_addr.len();

            let res = unsafe {
                libc::recvfrom(
                    self.io_handle.raw_fd(),
                    ptr as *mut libc::c_void,
                    len,
                    0,
                    self.socket_addr.as_ptr() as *mut libc::sockaddr,
                    &mut addrlen,
                )
            };

            if res >= 0 {
                // Safety: the kernel wrote `addrlen` bytes of valid address data.
                unsafe {
                    self.socket_addr.set_length(addrlen);
                }
                return Submission::Ready(Completion {
                    result: Ok(res as u32),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_READ,
                    libc::EV_ADD | libc::EV_ONESHOT,
                ));
            }

            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }

            return Submission::Ready(Completion {
                result: Err(err),
                flags: 0,
            });
        }
    }
}

#[cfg(windows)]
impl<B: BoundedIoBufMut> Submittable for RecvFrom<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::io;
        use windows_sys::Win32::Networking::WinSock::{WSA_IO_PENDING, WSAGetLastError, WSARecvFrom};

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;

        // Must reside in stable memory for the overlapped I/O path.
        self.socket_addr_len = self.socket_addr.len() as i32;

        let result = unsafe {
            WSARecvFrom(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_recv,
                &mut flags,
                self.socket_addr.as_ptr() as *mut _,
                &mut self.socket_addr_len,
                interest.as_mut_ptr(),
                None,
            )
        };

        if result == 0 {
            return Submission::Pending(interest);
        }

        let err = unsafe { WSAGetLastError() };
        if err == WSA_IO_PENDING {
            return Submission::Pending(interest);
        }

        Submission::Ready(Completion {
            result: Err(io::Error::from_raw_os_error(err)),
            flags: 0,
        })
    }
}

impl<B: BoundedIoBufMut> Completable for RecvFrom<B> {
    type Result = BufResult<(usize, SocketAddr), B>;

    fn complete(self, completion_result: Completion) -> Self::Result {
        let res = completion_result.result.map(|v| v as usize);
        let mut buf = self.buf;

        #[cfg(windows)]
        unsafe {
            // Sync the address length that the kernel wrote through the
            // stable `lpFromlen` pointer during the overlapped operation.
            self.socket_addr.set_length(self.socket_addr_len as _);
        }

        let socket_addr = (*self.socket_addr).as_socket();

        let res = res.map(|bytes_written| {
            let socket_addr: SocketAddr = socket_addr.unwrap();

            // The kernel wrote `bytes_written` bytes to the buffer.
            unsafe {
                buf.set_init(bytes_written);
            }

            (bytes_written, socket_addr)
        });

        (res, buf)
    }
}
