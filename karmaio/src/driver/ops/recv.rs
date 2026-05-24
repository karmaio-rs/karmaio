use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Recv<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Stable WSABUF allocation for Windows overlapped I/O.
    #[cfg(windows)]
    wsa_buf: windows_sys::Win32::Networking::WinSock::WSABUF,
}

impl<B: BoundedIoBufMut> Op<Recv<B>> {
    pub(crate) fn recv(io_handle: &SharedIoHandle, buf: B) -> std::io::Result<Op<Recv<B>>> {
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
        use crate::driver::backends::kqueue::Interest;

        loop {
            use std::io;

            let ptr = self.buf.stable_write_ptr();
            let len = self.buf.bytes_total();

            //TODO: Check if we need to get any flags from the user
            let res = unsafe { libc::recv(self.io_handle.raw_fd(), ptr as *mut libc::c_void, len, 0) };

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
impl<B: BoundedIoBufMut> Submittable for Recv<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::io;
        use windows_sys::Win32::Networking::WinSock::{WSA_IO_PENDING, WSAGetLastError, WSARecv};

        let socket = self.io_handle.raw_socket();

        let mut interest = Interest::new(socket as _);
        let mut flags = 0u32;
        let mut bytes_recv = 0u32;

        let result = unsafe {
            WSARecv(
                socket as _,
                &mut self.wsa_buf,
                1,
                &mut bytes_recv,
                &mut flags,
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
