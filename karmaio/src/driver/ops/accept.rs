use std::{io, net::SocketAddr};

#[cfg(windows)]
use std::os::windows::io::RawSocket;

use crate::{
    driver::{
        Submission,
        helpers::{io_handle::SharedIoHandle, socket::Socket},
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

#[cfg(windows)]
const ACCEPT_ADDR_LEN: u32 =
    (std::mem::size_of::<windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE>() + 16) as u32;
#[cfg(windows)]
const ACCEPT_ADDR_BUF_LEN: usize = ACCEPT_ADDR_LEN as usize * 2;

pub(crate) struct Accept {
    io_handle: SharedIoHandle,
    #[cfg(unix)]
    socketaddr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
    #[cfg(windows)]
    accepted_socket: Option<RawSocket>,
    #[cfg(windows)]
    socketaddr: Box<[u8; ACCEPT_ADDR_BUF_LEN]>,
}

impl Op<Accept> {
    pub(crate) fn accept(io_handle: &SharedIoHandle) -> io::Result<Self> {
        #[cfg(unix)]
        let socketaddr = Box::new((
            unsafe { std::mem::zeroed() },
            std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
        ));

        #[cfg(windows)]
        let socketaddr = Box::new([0; ACCEPT_ADDR_BUF_LEN]);

        let data = Accept {
            io_handle: io_handle.clone(),
            #[cfg(windows)]
            accepted_socket: None,
            socketaddr,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl Operable for Accept {}

#[cfg(target_os = "linux")]
impl Submittable for Accept {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};
        opcode::Accept::new(
            types::Fd(self.io_handle.raw_fd()),
            &mut self.socketaddr.0 as *mut _ as *mut _,
            &mut self.socketaddr.1,
        )
        .flags(libc::O_CLOEXEC)
        .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Accept {
    fn submit(&mut self) -> Submission {
        loop {
            let raw_fd = unsafe {
                libc::accept(
                    self.io_handle.raw_fd(),
                    &mut self.socketaddr.0 as *mut _ as *mut libc::sockaddr,
                    &mut self.socketaddr.1,
                )
            };

            // The syscall succeded and an FD has been assigned
            // Return completion with the success result
            if raw_fd >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(raw_fd as u32),
                    flags: 0,
                });
            };

            // Fd was not assigned, this means an error
            let err = std::io::Error::last_os_error();

            // This means we have nothing to accept yet. Register with the driver and wait
            if err.kind() == std::io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                use crate::driver::backends::kqueue::Interest;

                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_READ,
                    libc::EV_ADD | libc::EV_ONESHOT,
                ));
            }

            // The syscall was interrupted. Try again till we get success or error
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            // An actual error happened. Fire a completion with the error
            return Submission::Ready(Completion {
                result: Err(err),
                flags: 0,
            });
        }
    }
}

#[cfg(windows)]
impl Submittable for Accept {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::{AcceptEx, SOCKET, WSA_IO_PENDING, WSAGetLastError};

        let listen_socket = self.io_handle.raw_socket() as SOCKET;
        let accept_socket = match self.accepted_socket {
            Some(socket) => socket as SOCKET,
            None => {
                let socket = match create_accept_socket(listen_socket) {
                    Ok(socket) => socket,
                    Err(err) => {
                        return Submission::Ready(Completion {
                            result: Err(err),
                            flags: 0,
                        });
                    }
                };

                self.accepted_socket = Some(socket);
                socket as SOCKET
            }
        };

        let mut interest = Interest::new(listen_socket as _);
        let mut bytes_received = 0;
        let result = unsafe {
            AcceptEx(
                listen_socket,
                accept_socket,
                self.socketaddr.as_mut_ptr().cast(),
                0,
                ACCEPT_ADDR_LEN,
                ACCEPT_ADDR_LEN,
                &mut bytes_received,
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
            result: Err(io::Error::from_raw_os_error(err)),
            flags: 0,
        })
    }
}

#[cfg(unix)]
impl Completable for Accept {
    type Result = io::Result<(Socket, Option<SocketAddr>)>;

    fn complete(self, completion: Completion) -> Self::Result {
        let raw_fd = completion.result?;
        let io_handle = SharedIoHandle::new(raw_fd as i32);
        let socket = Socket::from(io_handle);

        let (_, addr) = unsafe {
            socket2::SockAddr::try_init(move |addr_storage, len| {
                let storage = &mut *addr_storage;
                let libc_storage = storage.view_as::<libc::sockaddr_storage>();
                *libc_storage = self.socketaddr.0;
                *len = self.socketaddr.1;
                Ok(())
            })?
        };

        Ok((socket, addr.as_socket()))
    }
}

#[cfg(windows)]
impl Completable for Accept {
    type Result = io::Result<(Socket, Option<SocketAddr>)>;

    fn complete(mut self, completion: Completion) -> Self::Result {
        use windows_sys::Win32::Networking::WinSock::{
            GetAcceptExSockaddrs, SO_UPDATE_ACCEPT_CONTEXT, SOCKADDR, SOCKET, SOL_SOCKET, setsockopt,
        };

        let listen_socket = self.io_handle.raw_socket() as SOCKET;
        let _ = completion.result?;
        let accepted_socket = self.accepted_socket.expect("missing accepted socket") as SOCKET;

        let context_result = unsafe {
            setsockopt(
                accepted_socket,
                SOL_SOCKET,
                SO_UPDATE_ACCEPT_CONTEXT,
                (&listen_socket as *const SOCKET).cast(),
                std::mem::size_of::<SOCKET>() as i32,
            )
        };

        if context_result != 0 {
            return Err(wsa_last_error());
        }

        let mut _local_addr = std::ptr::null_mut::<SOCKADDR>();
        let mut _local_addr_len = 0;
        let mut remote_addr = std::ptr::null_mut::<SOCKADDR>();
        let mut remote_addr_len = 0;

        unsafe {
            GetAcceptExSockaddrs(
                self.socketaddr.as_ptr().cast(),
                0,
                ACCEPT_ADDR_LEN,
                ACCEPT_ADDR_LEN,
                &mut _local_addr,
                &mut _local_addr_len,
                &mut remote_addr,
                &mut remote_addr_len,
            );
        }

        let (_, addr) = unsafe {
            socket2::SockAddr::try_init(|storage, len| {
                std::ptr::copy_nonoverlapping(remote_addr.cast::<u8>(), storage.cast::<u8>(), remote_addr_len as usize);
                *len = remote_addr_len as _;
                Ok(())
            })?
        };

        let accepted_socket = self.accepted_socket.take().expect("missing accepted socket");
        let io_handle = SharedIoHandle::new_socket(accepted_socket);
        let socket = Socket::from(io_handle);

        Ok((socket, addr.as_socket()))
    }
}

#[cfg(windows)]
impl Drop for Accept {
    fn drop(&mut self) {
        if let Some(socket) = self.accepted_socket.take() {
            unsafe {
                windows_sys::Win32::Networking::WinSock::closesocket(socket as _);
            }
        }
    }
}

#[cfg(windows)]
fn create_accept_socket(listen_socket: windows_sys::Win32::Networking::WinSock::SOCKET) -> io::Result<RawSocket> {
    use windows_sys::Win32::Networking::WinSock::{
        INVALID_SOCKET, IPPROTO_TCP, SOCK_STREAM, SOCKADDR, SOCKADDR_STORAGE, SOCKET_ERROR, WSA_FLAG_OVERLAPPED,
        WSASocketW, getsockname,
    };

    let mut addr = SOCKADDR_STORAGE::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    let result = unsafe {
        getsockname(
            listen_socket,
            (&mut addr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            &mut addr_len,
        )
    };

    if result == SOCKET_ERROR {
        return Err(wsa_last_error());
    }

    let socket = unsafe {
        WSASocketW(
            addr.ss_family as i32,
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED,
        )
    };

    if socket == INVALID_SOCKET {
        Err(wsa_last_error())
    } else {
        Ok(socket as RawSocket)
    }
}

#[cfg(windows)]
fn wsa_last_error() -> io::Error {
    let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
    io::Error::from_raw_os_error(err)
}
