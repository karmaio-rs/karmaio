use std::net::SocketAddr;

#[cfg(unix)]
use std::io::IoSlice;

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct SendMsg<B: BoundedIoBuf, C: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Held so the sockaddr remains valid for the kernel while the op is in-flight.
    #[allow(dead_code)]
    socket_addr: Option<Box<SockAddr>>,

    // Reference to the in-flight buffer.
    pub(crate) bufs: Vec<B>,

    pub(crate) control: Option<C>,

    // Held so the iovec memory stays valid for the kernel while the op is in-flight.
    #[cfg(unix)]
    #[allow(dead_code)]
    io_slices: Vec<IoSlice<'static>>,

    // Pointer to the msghdr struct sent to the kernel
    #[cfg(unix)]
    pub(crate) msghdr: Box<libc::msghdr>,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl<B: BoundedIoBuf, C: BoundedIoBuf> Op<SendMsg<B, C>> {
    pub(crate) fn sendmsg(
        io_handle: &SharedIoHandle<socket2::Socket>,
        bufs: Vec<B>,
        control: Option<C>,
        socket_addr: Option<SocketAddr>,
    ) -> std::io::Result<Op<SendMsg<B, C>>> {
        let socket_addr = socket_addr.map(|addr| Box::new(SockAddr::from(addr)));

        #[cfg(unix)]
        let (io_slices, msghdr) = {
            let mut io_slices = Vec::with_capacity(bufs.len());
            for buf in &bufs {
                io_slices.push(IoSlice::new(unsafe {
                    std::slice::from_raw_parts(buf.stable_read_ptr(), buf.bytes_init())
                }));
            }

            let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msghdr.msg_iov = io_slices.as_mut_ptr().cast();
            msghdr.msg_iovlen = io_slices.len() as _;

            match &socket_addr {
                Some(addr) => {
                    msghdr.msg_name = addr.as_ptr() as *mut libc::c_void;
                    msghdr.msg_namelen = addr.len();
                }
                None => {
                    msghdr.msg_name = std::ptr::null_mut();
                    msghdr.msg_namelen = 0;
                }
            }

            match &control {
                Some(msg_control) => {
                    msghdr.msg_control = msg_control.stable_read_ptr() as *mut _;
                    msghdr.msg_controllen = msg_control.bytes_init() as _;
                }
                None => {
                    msghdr.msg_control = std::ptr::null_mut();
                    msghdr.msg_controllen = 0;
                }
            }

            (io_slices, msghdr)
        };

        #[cfg(windows)]
        let wsa_bufs: Vec<windows_sys::Win32::Networking::WinSock::WSABUF> = bufs
            .iter()
            .map(|buf| windows_sys::Win32::Networking::WinSock::WSABUF {
                len: buf.bytes_init() as u32,
                buf: buf.stable_read_ptr() as *mut u8,
            })
            .collect();

        let data = SendMsg {
            io_handle: io_handle.clone(),
            bufs,
            socket_addr,
            #[cfg(unix)]
            io_slices,
            control,
            #[cfg(unix)]
            msghdr,
            #[cfg(windows)]
            wsa_bufs,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBuf, C: BoundedIoBuf> Operable for SendMsg<B, C> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf, C: BoundedIoBuf> Submittable for SendMsg<B, C> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::SendMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_ref() as *const _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf, C: BoundedIoBuf> Submittable for SendMsg<B, C> {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_WRITE, {
            macos_syscall!(libc::sendmsg(
                self.io_handle.raw_fd(),
                self.msghdr.as_ref() as *const libc::msghdr,
                0,
            ))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf, C: BoundedIoBuf> Submittable for SendMsg<B, C> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASendTo;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

        let (name, namelen) = match &self.socket_addr {
            Some(addr) => (addr.as_ptr() as *const _, addr.len() as i32),
            None => (std::ptr::null(), 0),
        };

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
}

impl<B: BoundedIoBuf, C: BoundedIoBuf> Completable for SendMsg<B, C> {
    type Result = BufResult<(usize, Option<C>), Vec<B>>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);

        // Recover the buffers
        let bufs = self.bufs;

        let res = res.map(|bytes_written| {
            // Recover the ancillary data buffer.
            let control = self.control;

            (bytes_written, control)
        });

        (res, bufs)
    }
}
