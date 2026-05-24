use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use std::io::IoSlice;

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

// Implementation Notes -
//
// On Linux, iouring does not support sendto yet, so have to simulate it with sendmsg
// On Windows and macOS, the standard sendto syscalls are used
pub(crate) struct SendTo<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle,

    socket_addr: Box<SockAddr>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Internal pointers to the IOVEC strcuts
    #[cfg(target_os = "linux")]
    io_slices: Vec<IoSlice<'static>>,

    // Pointer to the msghdr struct sent to the kernel
    #[cfg(target_os = "linux")]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: windows_sys::Win32::Networking::WinSock::WSABUF,
}

impl<B: BoundedIoBuf> Op<SendTo<B>> {
    pub(crate) fn send_to(
        io_handle: &SharedIoHandle,
        buf: B,
        socket_addr: SocketAddr,
    ) -> std::io::Result<Op<SendTo<B>>> {
        let socket_addr = Box::new(SockAddr::from(socket_addr));

        #[cfg(target_os = "linux")]
        let (io_slices, msghdr) = {
            let mut io_slices = vec![IoSlice::new(unsafe {
                std::slice::from_raw_parts(buf.stable_read_ptr(), buf.bytes_init())
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
            len: buf.bytes_init() as u32,
            buf: buf.stable_read_ptr() as *mut u8,
        };

        let data = SendTo {
            io_handle: io_handle.clone(),
            buf,
            socket_addr,
            #[cfg(target_os = "linux")]
            io_slices,
            #[cfg(target_os = "linux")]
            msghdr,
            #[cfg(windows)]
            wsa_buf,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBuf> Operable for SendTo<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> Submittable for SendTo<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::SendMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_ref() as *const _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf> Submittable for SendTo<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::kqueue::Interest;

        loop {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();

            let name = self.socket_addr.as_ptr() as *const libc::sockaddr;
            let namelen = self.socket_addr.len();

            let res = unsafe {
                libc::sendto(
                    self.io_handle.raw_fd(),
                    ptr as *const libc::c_void,
                    len,
                    0,
                    name,
                    namelen,
                )
            };

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
                    libc::EVFILT_WRITE,
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
impl<B: BoundedIoBuf> Submittable for SendTo<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::io;
        use windows_sys::Win32::Networking::WinSock::{WSA_IO_PENDING, WSAGetLastError, WSASendTo};

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

        let name = self.socket_addr.as_ptr() as *const _;
        let namelen = self.socket_addr.len() as i32;

        let result = unsafe {
            WSASendTo(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_sent,
                0,
                name,
                namelen,
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

impl<B: BoundedIoBuf> Completable for SendTo<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}
