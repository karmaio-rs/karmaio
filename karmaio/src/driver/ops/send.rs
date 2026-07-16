use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Send<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: windows_sys::Win32::Networking::WinSock::WSABUF,
}

impl<B: BoundedIoBuf> Op<Send<B>> {
    pub(crate) fn send(io_handle: &SharedIoHandle, buf: B) -> std::io::Result<Op<Send<B>>> {
        #[cfg(windows)]
        let wsa_buf = windows_sys::Win32::Networking::WinSock::WSABUF {
            len: buf.bytes_init() as u32,
            buf: buf.stable_read_ptr() as *mut u8,
        };

        let data = Send {
            io_handle: io_handle.clone(),
            buf,
            #[cfg(windows)]
            wsa_buf,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBuf> Operable for Send<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> Submittable for Send<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init();

        opcode::Send::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _).build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf> Submittable for Send<B> {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_WRITE, {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();

            macos_syscall!(libc::send(self.io_handle.raw_fd(), ptr as *const libc::c_void, len, 0,))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> Submittable for Send<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::WSASend;

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

        windows_syscall_submit_overlapped!(interest, socket, {
            WSASend(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_sent,
                0,
                interest.as_mut_ptr(),
                None,
            )
        })
    }
}

impl<B: BoundedIoBuf> Completable for Send<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}
