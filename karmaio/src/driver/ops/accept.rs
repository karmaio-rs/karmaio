use std::{io, net::SocketAddr};

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;

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
        helpers::{io_handle::SharedIoHandle, socket::Socket},
        ops::{Completion, Op},
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
    #[cfg(target_os = "linux")]
    socketaddr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
    /// Owned accepted socket claimed as soon as the platform produces it, so
    /// later failures in `complete` still close the descriptor via `Drop`.
    #[cfg(target_os = "linux")]
    accepted_socket: Option<socket2::Socket>,
    #[cfg(windows)]
    accepted_socket: Option<RawSocket>,
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    accepted: Option<(OwnedFd, Option<rustix::net::SocketAddrAny>)>,
    #[cfg(windows)]
    socketaddr: Box<[u8; ACCEPT_ADDR_BUF_LEN]>,
}

impl Op<Accept> {
    pub(crate) fn accept(io_handle: &SharedIoHandle<socket2::Socket>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let socketaddr = Box::new((
            unsafe { std::mem::zeroed() },
            std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
        ));

        #[cfg(windows)]
        let socketaddr = Box::new([0; ACCEPT_ADDR_BUF_LEN]);

        let data = Accept {
            io_handle: io_handle.clone(),
            #[cfg(any(target_os = "linux", windows))]
            accepted_socket: None,
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "dragonfly"
            ))]
            accepted: None,
            #[cfg(any(target_os = "linux", windows))]
            socketaddr,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for Accept {
    type Output = io::Result<(Socket, Option<SocketAddr>)>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};
        opcode::Accept::new(
            types::Fd(self.io_handle.raw_fd()),
            &mut self.socketaddr.0 as *mut _ as *mut _,
            &mut self.socketaddr.1,
        )
        .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
        .build()
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        use std::os::fd::{FromRawFd, RawFd};

        // Claim the accepted fd before any fallible post-processing so Drop
        // still closes it if a later step fails (or the consumer has detached).
        let raw_fd = completion.result? as RawFd;
        // Safety: a successful Accept CQE transfers ownership of one new
        // socket descriptor to this operation.
        self.accepted_socket = Some(unsafe { socket2::Socket::from_raw_fd(raw_fd) });

        let sock = self
            .accepted_socket
            .take()
            .expect("successful accept completion missing owned socket");
        let socket = Socket::from_socket(sock)?;

        socket.set_async_flags()?;

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

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for Accept {
    type Output = io::Result<(Socket, Option<SocketAddr>)>;
    fn attempt(&mut self) -> KqueueAttempt {
        use rustix::net::acceptfrom;

        kqueue_syscall_submit!(
            self.io_handle.raw_fd(),
            crate::driver::backends::kqueue::Direction::Read,
            {
                acceptfrom(&self.io_handle)
                    .map(|accepted| {
                        self.accepted = Some(accepted);
                        0_u32
                    })
                    .map_err(std::io::Error::from)
            }
        )
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        let _ = completion.result?;
        let (accepted, address) = self
            .accepted
            .take()
            .ok_or_else(|| io::Error::other("accepted socket missing after successful accept"))?;
        let sock = socket2::Socket::from(accepted);
        let socket = Socket::from_socket(sock)?;

        socket.set_async_flags()?;
        let address = address.map(SocketAddr::try_from).transpose().map_err(io::Error::from)?;

        Ok((socket, address))
    }
}

#[cfg(windows)]
unsafe impl IocpOperation for Accept {
    type Output = io::Result<(Socket, Option<SocketAddr>)>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Networking::WinSock::{AcceptEx, SOCKET};

        let listen_socket = self.io_handle.raw_socket() as SOCKET;
        let accept_socket = match self.accepted_socket {
            Some(socket) => socket as SOCKET,
            None => {
                let socket = match create_accept_socket(listen_socket) {
                    Ok(socket) => socket,
                    Err(err) => {
                        return IocpSubmission::Ready(Completion::new(Err(err)));
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

    fn complete(mut self, completion: Completion) -> Self::Output {
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
        let socket = Socket::from_socket(sock)?;

        let _ = socket.set_async_flags();

        Ok((socket, addr.as_socket()))
    }
}

impl Drop for Accept {
    fn drop(&mut self) {
        // Linux: `accepted_socket` is a real `Socket` and closes itself on drop.
        // Windows: close the pre-allocated AcceptEx socket if complete never took it.
        // kqueue: `accepted` is `OwnedFd` and closes itself on drop.
        #[cfg(windows)]
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
