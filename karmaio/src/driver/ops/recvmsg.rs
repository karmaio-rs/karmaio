use std::{io::IoSliceMut, net::SocketAddr};

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

pub(crate) struct RecvMsg<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle,

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
    socket_addr_len: i32,
}

impl<B: BoundedIoBufMut> Op<RecvMsg<B>> {
    pub(crate) fn recvmsg(io_handle: &SharedIoHandle, mut bufs: Vec<B>) -> std::io::Result<Op<RecvMsg<B>>> {
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
            msghdr.msg_namelen = socket_addr.len() as u32;
            msghdr
        };

        #[cfg(windows)]
        let wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF> = bufs
            .iter()
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
            socket_addr_len: 0,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBufMut> Operable for RecvMsg<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBufMut> Submittable for RecvMsg<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut() as *mut _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> Submittable for RecvMsg<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::kqueue::Interest;

        loop {
            let res = unsafe { libc::recvmsg(self.io_handle.raw_fd(), self.msghdr.as_mut() as *mut libc::msghdr, 0) };

            if res >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(res as u32),
                    flags: 0,
                });
            }

            let err = std::io::Error::last_os_error();

            if err.kind() == std::io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_READ,
                    libc::EV_ADD | libc::EV_ONESHOT,
                ));
            }

            if err.kind() == std::io::ErrorKind::Interrupted {
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
impl<B: BoundedIoBufMut> Submittable for RecvMsg<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::io;
        use windows_sys::Win32::Networking::WinSock::{WSA_IO_PENDING, WSAGetLastError, WSARecvFrom};

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_recv = 0u32;
        let mut flags = 0u32;

        self.socket_addr_len = self.socket_addr.len() as i32;

        let result = unsafe {
            WSARecvFrom(
                socket as _,
                self.wsa_bufs.as_ptr() as *const _,
                self.wsa_bufs.len() as u32,
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

impl<B: BoundedIoBufMut> Completable for RecvMsg<B> {
    type Result = BufResult<(usize, SocketAddr), Vec<B>>;

    fn complete(self, completion_result: Completion) -> Self::Result {
        let res = completion_result.result.map(|v| v as usize);
        let mut bufs = self.bufs;

        #[cfg(windows)]
        unsafe {
            self.socket_addr.set_length(self.socket_addr_len as _);
        }

        let socket_addr = (*self.socket_addr).as_socket();

        let res = res.map(|total_bytes_written| {
            let socket_addr: SocketAddr = socket_addr.unwrap();

            // The kernel wrote `total_bytes_written` bytes to the buffers.
            // The kernel fills buffers to their capacity one after the other
            // So we do a loop to correctly update buffer lengths on our end.
            let mut remaining = total_bytes_written;

            for buf in &mut bufs {
                // Grab the filled capacity of the buffer
                let bytes_written = std::cmp::min(remaining, buf.bytes_total());

                unsafe {
                    // Set the length of the buffers
                    buf.set_init(bytes_written);
                }
                // Find the remaining bytes
                remaining -= bytes_written;

                // The current buffer is the last filled buffer.
                // We can return. The rest of the buffers are empty
                if remaining == 0 {
                    break;
                }
            }

            (total_bytes_written, socket_addr)
        });

        (res, bufs)
    }
}
