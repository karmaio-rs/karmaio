use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Recv<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: windows_sys::Win32::Networking::WinSock::WSABUF,
}

impl<B: BoundedIoBufMut> Op<Recv<B>> {
    // `mut buf` is required on Windows (`stable_write_ptr`); unused on other targets.
    #[allow(unused_mut)]
    pub(crate) fn recv(io_handle: &SharedIoHandle<socket2::Socket>, mut buf: B) -> std::io::Result<Op<Recv<B>>> {
        #[cfg(windows)]
        let wsa_buf = windows_sys::Win32::Networking::WinSock::WSABUF {
            len: buf.bytes_total() as u32,
            buf: buf.stable_write_ptr() as *mut u8,
        };

        let data = Recv {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBufMut> Operable for Recv<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBufMut> Submittable for Recv<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total();

        opcode::Recv::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> Submittable for Recv<B> {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            let ptr = self.buf.stable_write_ptr();
            let len = self.buf.bytes_total();

            // TODO: Check if we need to get any flags from the user
            macos_syscall!(libc::recv(self.io_handle.raw_fd(), ptr as *mut libc::c_void, len, 0))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBufMut> Submittable for Recv<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSARecv;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSARecv(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_recv,
                &mut flags,
                interest.as_mut_ptr(),
                None,
            )
        })
    }
}

impl<B: BoundedIoBufMut> Completable for Recv<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut buf = self.buf;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            // Safety: the kernel wrote `n` bytes to the buffer.
            unsafe {
                buf.set_init(n);
            }
        }

        (res, buf)
    }
}
