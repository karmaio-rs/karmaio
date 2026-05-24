use socket2::SockAddr;

use crate::{
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

/// Open a file
pub(crate) struct Connect {
    io_handle: SharedIoHandle,
    // this avoids a UAF (UAM?) if the future is moved, but not if the future is dropped.
    // No Op can be dropped before completion right now.
    socket_addr: Box<SockAddr>,
}

impl Op<Connect> {
    /// Submit a request to connect.
    pub(crate) fn connect(io_handle: &SharedIoHandle, socket_addr: SockAddr) -> std::io::Result<Op<Connect>> {
        let data = Connect {
            io_handle: io_handle.clone(),
            socket_addr: Box::new(socket_addr),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Connect {}

#[cfg(target_os = "linux")]
impl Submittable for Connect {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::Connect::new(
            types::Fd(self.io_handle.raw_fd()),
            self.socket_addr.as_ptr(),
            self.socket_addr.len(),
        )
        .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Connect {
    fn submit(&mut self) -> Submission {
        loop {
            let ret = unsafe {
                libc::connect(
                    self.io_handle.raw_fd(),
                    self.socket_addr.as_ptr() as *const libc::sockaddr,
                    self.socket_addr.len(),
                )
            };

            if ret == 0 {
                return Submission::Ready(Completion {
                    result: Ok(0),
                    flags: 0,
                });
            }

            let err = std::io::Error::last_os_error();

            if err.raw_os_error() == Some(libc::EISCONN) {
                return Submission::Ready(Completion {
                    result: Ok(0),
                    flags: 0,
                });
            }

            if err.raw_os_error() == Some(libc::EINPROGRESS) || err.kind() == std::io::ErrorKind::WouldBlock {
                use crate::driver::backends::kqueue::Interest;
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
impl Submittable for Connect {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use std::{mem, ptr, sync::OnceLock};
        use windows_sys::Win32::Networking::WinSock::{
            LPFN_CONNECTEX, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKET, WSA_IO_PENDING, WSAGetLastError,
            WSAID_CONNECTEX, WSAIoctl,
        };

        let socket = self.io_handle.raw_socket() as SOCKET;

        // ConnectEx is not exported directly from ws2_32.dll — it must be
        // resolved at runtime via WSAIoctl with SIO_GET_EXTENSION_FUNCTION_POINTER.
        static CONNECTEX: OnceLock<LPFN_CONNECTEX> = OnceLock::new();
        let connectex = *CONNECTEX.get_or_init(|| {
            let mut ptr: LPFN_CONNECTEX = None;
            let mut bytes = 0u32;
            let result = unsafe {
                WSAIoctl(
                    socket,
                    SIO_GET_EXTENSION_FUNCTION_POINTER,
                    &WSAID_CONNECTEX as *const _ as *const core::ffi::c_void,
                    mem::size_of::<windows_sys::core::GUID>() as u32,
                    &mut ptr as *mut _ as *mut core::ffi::c_void,
                    mem::size_of::<LPFN_CONNECTEX>() as u32,
                    &mut bytes,
                    ptr::null_mut(),
                    None,
                )
            };
            if result != 0 {
                panic!(
                    "WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER, WSAID_CONNECTEX) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            ptr.expect("WSAIoctl returned success but ConnectEx pointer is null")
        });

        // TODO: ConnectEx may require the socket to be explicitly bound first.
        // If the socket was created without an explicit bind, ConnectEx can fail
        // with WSAEINVAL on some Windows configurations. Consider adding a bind
        // to INADDR_ANY:0 before calling ConnectEx.
        let connectex = connectex.expect("ConnectEx not loaded");
        let mut interest = Interest::new(socket as _);

        let result = unsafe {
            connectex(
                socket,
                self.socket_addr.as_ptr() as *const _,
                self.socket_addr.len() as i32,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                interest.as_mut_ptr(),
            )
        };

        if result != 0 {
            // Overlapped operations on IOCP still produce a completion packet for synchronous success,
            // so the driver keeps this OVERLAPPED allocation alive and waits for that packet.
            return Submission::Pending(interest);
        }

        let err = unsafe { WSAGetLastError() };
        if err == WSA_IO_PENDING {
            return Submission::Pending(interest);
        }

        Submission::Ready(Completion {
            result: Err(std::io::Error::from_raw_os_error(err)),
            flags: 0,
        })
    }
}

#[cfg(unix)]
impl Completable for Connect {
    type Result = std::io::Result<()>;

    fn complete(self, completion_entry: Completion) -> Self::Result {
        completion_entry.result.map(|_| ())
    }
}

#[cfg(windows)]
impl Completable for Connect {
    type Result = std::io::Result<()>;

    fn complete(self, completion_entry: Completion) -> Self::Result {
        use windows_sys::Win32::Networking::WinSock::{SO_UPDATE_CONNECT_CONTEXT, SOCKET, SOL_SOCKET, setsockopt};

        completion_entry.result?;

        let socket = self.io_handle.raw_socket() as SOCKET;
        let result = unsafe { setsockopt(socket, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, std::ptr::null(), 0) };

        if result == 0 {
            Ok(())
        } else {
            let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
            Err(std::io::Error::from_raw_os_error(err))
        }
    }
}
