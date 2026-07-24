use std::{io, net::SocketAddr};

#[cfg(windows)]
use std::os::windows::io::RawSocket;

use crate::{
    driver::{
        Submission,
        helpers::{attached_handle::AttachedHandle, io_handle::SharedIoHandle, socket::Socket},
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
    io_handle: SharedIoHandle<socket2::Socket>,
    #[cfg(unix)]
    socketaddr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
    #[cfg(windows)]
    accepted_socket: Option<RawSocket>,
    #[cfg(windows)]
    socketaddr: Box<[u8; ACCEPT_ADDR_BUF_LEN]>,
}

impl Op<Accept> {
    pub(crate) fn accept(io_handle: &SharedIoHandle<socket2::Socket>) -> io::Result<Self> {
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
        .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
        .build()
    }
}

#[cfg(target_os = "macos")]
impl Submittable for Accept {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            macos_syscall!(libc::accept(
                self.io_handle.raw_fd(),
                &mut self.socketaddr.0 as *mut _ as *mut libc::sockaddr,
                &mut self.socketaddr.1,
            ))
        })
    }
}

#[cfg(windows)]
impl Submittable for Accept {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::{AcceptEx, SOCKET};

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

        // Overlapped operations on IOCP still produce a completion packet for synchronous success,
        // so the driver keeps this OVERLAPPED allocation alive and waits for that packet.
        windows_syscall_submit_overlapped!(interest, winsock, {
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
        })
    }
}

#[cfg(unix)]
impl Completable for Accept {
    type Result = io::Result<(Socket, Option<SocketAddr>)>;

    fn complete(self, completion: Completion) -> Self::Result {
        use std::os::fd::{FromRawFd, RawFd};

        let raw_fd = completion.result? as RawFd;
        // Safety: accept returned a new open socket fd; ownership transfers here.
        let sock = unsafe { socket2::Socket::from_raw_fd(raw_fd) };
        let socket = Socket {
            // SAFETY: The socket was just accepted and will be used within the runtime context.
            handle: unsafe { AttachedHandle::new_unchecked(sock) },
        };

        let _ = socket.set_async_flags();

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
        use std::os::windows::io::FromRawSocket;
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
        // Safety: AcceptEx produced an open socket; ownership transfers here.
        let sock = unsafe { socket2::Socket::from_raw_socket(accepted_socket) };
        let socket = Socket {
            // SAFETY: The socket was just accepted and will be used within the runtime context.
            handle: unsafe { AttachedHandle::new_unchecked(sock) },
        };

        let _ = socket.set_async_flags();

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
