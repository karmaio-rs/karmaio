use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
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
        use crate::driver::backends::kqueue::Interest;
        use std::io;

        loop {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();

            let res = unsafe { libc::send(self.io_handle.raw_fd(), ptr as *const libc::c_void, len, 0) };

            if res >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(res as u32),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_WRITE,
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
impl<B: BoundedIoBuf> Submittable for Send<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::io;
        use windows_sys::Win32::Networking::WinSock::{WSA_IO_PENDING, WSAGetLastError, WSASend};

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut bytes_sent = 0u32;

        let result = unsafe {
            WSASend(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_sent,
                0,
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

impl<B: BoundedIoBuf> Completable for Send<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        let res = completion_entry.result.map(|v| v as usize);
        let buf = self.buf;

        (res, buf)
    }
}
