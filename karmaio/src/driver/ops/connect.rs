use socket2::SockAddr;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::driver::backends::kqueue::{KqueueAttempt, KqueueOperation};

use crate::{
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

/// Open a file
pub(crate) struct Connect {
    io_handle: SharedIoHandle<socket2::Socket>,
    // this avoids a UAF (UAM?) if the future is moved, but not if the future is dropped.
    // No Op can be dropped before completion right now.
    socket_addr: Box<SockAddr>,
}

impl Op<Connect> {
    /// Submit a request to connect.
    pub(crate) fn connect(
        io_handle: &SharedIoHandle<socket2::Socket>,
        socket_addr: SockAddr,
    ) -> std::io::Result<Op<Connect>> {
        let data = Connect {
            io_handle: io_handle.clone(),
            socket_addr: Box::new(socket_addr),
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Connect {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::Connect::new(
            types::Fd(self.io_handle.raw_fd()),
            self.socket_addr.as_ptr() as *const _,
            self.socket_addr.len() as u32,
        )
        .build()
    }

    fn complete(self, completion_entry: Completion) -> Self::Output {
        completion_entry.result.map(|_| ())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Connect {
    type Output = std::io::Result<()>;
    fn attempt(&mut self) -> KqueueAttempt {
        kqueue_syscall_submit!(connect self.io_handle.raw_fd(), {
            kqueue_syscall!(libc::connect(
                self.io_handle.raw_fd(),
                self.socket_addr.as_ptr() as *const libc::sockaddr,
                self.socket_addr.len(),
            ))
        })
    }

    fn complete(self, completion_entry: Completion) -> Self::Output {
        completion_entry.result.map(|_| ())
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Connect {
    type Output = std::io::Result<()>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use std::{mem, ptr, sync::OnceLock};
        use windows_sys::Win32::Networking::WinSock::{
            LPFN_CONNECTEX, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKET, WSAID_CONNECTEX, WSAIoctl,
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
            Some(ptr.expect("WSAIoctl returned success but ConnectEx pointer is null"))
        });

        let connectex = connectex.expect("ConnectEx not loaded");
        let mut interest = Interest::new(socket as _);

        // Overlapped operations on IOCP still produce a completion packet for synchronous success,
        // so the driver keeps this OVERLAPPED allocation alive and waits for that packet.
        windows_syscall_submit_overlapped!(interest, winsock, {
            connectex(
                socket,
                self.socket_addr.as_ptr() as *const _,
                self.socket_addr.len() as i32,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                interest.as_mut_ptr(),
            )
        })
    }

    fn complete(self, completion_entry: Completion) -> Self::Output {
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
