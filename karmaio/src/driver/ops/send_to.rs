use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use std::io::IoSlice;

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{BackendComplete, BackendSubmission, BackendSubmit, Op},
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

impl<B: BoundedIoBuf> Op<SendTo<B>> {
    pub(crate) fn send_to(
        io_handle: &SharedIoHandle<socket2::Socket>,
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
            wsa_buf: Box::new(wsa_buf),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> BackendSubmit for SendTo<B> {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};

        opcode::SendMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_ref() as *const _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf> BackendSubmit for SendTo<B> {
    fn submit(&mut self) -> BackendSubmission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_WRITE, {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();
            let name = self.socket_addr.as_ptr() as *const libc::sockaddr;
            let namelen = self.socket_addr.len();

            macos_syscall!(libc::sendto(
                self.io_handle.raw_fd(),
                ptr as *const libc::c_void,
                len,
                0,
                name,
                namelen,
            ))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> BackendSubmit for SendTo<B> {
    fn submit(&mut self) -> BackendSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASendTo;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

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
}

impl<B: BoundedIoBuf> BackendComplete for SendTo<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}
