//! Owned TCP stream splitting: `into_split`, reunification, and half lifecycle.

use std::{cell::Cell, net::SocketAddr, rc::Rc};

use karmaio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use karmaio::net::split::{ReuniteErrorKind, ReuniteOwned};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::spawn_local;

#[path = "common/partial_io.rs"]
mod partial_io;

use partial_io::{PartialReader, PartialWriter};

const PAYLOAD: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()";

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[karmaio::test]
async fn owned_halves_used_concurrently_in_separate_tasks() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(PAYLOAD[..16].to_vec()).await.unwrap();
        let (res, buf) = socket.read_exact(Box::new([0u8; 16])).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], &PAYLOAD[16..32]);
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();

    let reader = spawn_local(async move {
        let (res, buf) = read_half.read_exact(Box::new([0u8; 16])).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], &PAYLOAD[..16]);
    });
    let writer = spawn_local(async move {
        write_half.write_all(PAYLOAD[16..32].to_vec()).await.unwrap();
    });

    writer.await.unwrap();
    reader.await.unwrap();
    server.await.unwrap();
}

#[karmaio::test]
async fn partial_reads_and_writes_span_multiple_operations() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(PAYLOAD[..33].to_vec()).await.unwrap();
        socket.write_all(PAYLOAD[33..66].to_vec()).await.unwrap();
        socket.write_all(PAYLOAD[66..].to_vec()).await.unwrap();

        let mut got = Vec::new();
        loop {
            let (res, buf) = socket.read(Box::new([0u8; 7])).await.into_parts();
            match res.unwrap() {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, PAYLOAD);
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let read_calls = Rc::new(Cell::new(0));
    let write_calls = Rc::new(Cell::new(0));

    let reader_calls = Rc::clone(&read_calls);
    let reader = spawn_local(async move {
        let mut read_half = PartialReader::new(read_half, 7, reader_calls);
        let (res, buf) = read_half.read_exact(Box::new([0u8; PAYLOAD.len()])).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], PAYLOAD);
    });

    let writer_calls = Rc::clone(&write_calls);
    let writer = spawn_local(async move {
        let mut write_half = PartialWriter::new(write_half, 11, writer_calls);
        write_half.write_all(PAYLOAD.to_vec()).await.unwrap();
        write_half.shutdown().await.unwrap();
    });

    writer.await.unwrap();
    reader.await.unwrap();
    server.await.unwrap();
    assert!(read_calls.get() > 1, "read must span multiple owned-half operations");
    assert!(write_calls.get() > 1, "write must span multiple owned-half operations");
}

#[karmaio::test]
async fn dropping_read_half_keeps_write_half_usable() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (res, buf) = socket.read_exact(Box::new([0u8; 4])).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"data");
        socket.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();

    drop(read_half);
    write_half.write_all(b"data".to_vec()).await.unwrap();
    write_half.shutdown().await.unwrap();

    server.await.unwrap();
}

#[karmaio::test]
async fn dropping_write_half_half_closes_but_reads_continue() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // The client drops its write half: we observe EOF on read...
        let mut got = Vec::new();
        loop {
            let (res, buf) = socket.read(Box::new([0u8; 64])).await.into_parts();
            match res.unwrap() {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, b"request");
        // ...while the client's read half must still receive from us.
        socket.write_all(b"response".to_vec()).await.unwrap();
        socket.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();

    write_half.write_all(b"request".to_vec()).await.unwrap();
    drop(write_half);

    let (res, buf) = read_half.read_exact(Box::new([0u8; 8])).await.into_parts();
    res.unwrap();
    assert_eq!(&buf[..], b"response");

    server.await.unwrap();
}

#[karmaio::test]
async fn explicit_write_half_shutdown_then_reads_succeed() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let got = {
            let mut out = Vec::new();
            loop {
                let (res, buf) = socket.read(Box::new([0u8; 64])).await.into_parts();
                match res.unwrap() {
                    0 => break,
                    n => out.extend_from_slice(&buf[..n]),
                }
            }
            out
        };
        assert_eq!(got, b"hello");
        socket.write_all(b"world".to_vec()).await.unwrap();
        socket.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();

    write_half.write_all(b"hello".to_vec()).await.unwrap();
    write_half.shutdown().await.unwrap();

    let (res, buf) = read_half.read_exact(Box::new([0u8; 5])).await.into_parts();
    res.unwrap();
    assert_eq!(&buf[..], b"world");

    server.await.unwrap();
}

#[karmaio::test]
async fn matching_halves_can_be_reunited_repeatedly() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        socket.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();

    let mut stream = stream;
    for _ in 0..3 {
        let (read_half, write_half) = stream.into_split();
        stream = <TcpStream as ReuniteOwned>::reunite(read_half, write_half).unwrap();
    }

    // Reunification leaves no extra ownership: close waits for uniqueness and
    // must not hang.
    stream.close().await.unwrap();
    server.await.unwrap();
}

#[karmaio::test]
async fn mismatched_halves_fail_reunite_without_resource_loss() {
    let (listener, addr) = bind();

    let server = spawn_local(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"hello".to_vec()).await.unwrap();
            socket.close().await.unwrap();
        }
    });

    let stream1 = TcpStream::connect(addr).await.unwrap();
    let stream2 = TcpStream::connect(addr).await.unwrap();
    let (read_half1, write_half1) = stream1.into_split();
    let (read_half2, write_half2) = stream2.into_split();

    let error = match read_half1.reunite(write_half2) {
        Ok(_) => panic!("mismatched halves must not reunite"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ReuniteErrorKind::Mismatched);
    let (read_half1, write_half2) = error.into_halves();

    // Both halves of the mismatched pair remain intact: correct pairing still
    // succeeds and the reconstructed streams are fully usable.
    let mut stream1 = read_half1.reunite(write_half1).unwrap();
    let mut stream2 = read_half2.reunite(write_half2).unwrap();

    let (res, buf) = stream1.read_exact(Box::new([0u8; 5])).await.into_parts();
    res.unwrap();
    assert_eq!(&buf[..], b"hello");
    let (res, buf) = stream2.read_exact(Box::new([0u8; 5])).await.into_parts();
    res.unwrap();
    assert_eq!(&buf[..], b"hello");

    server.await.unwrap();
}

// io_uring retains detached one-shot operations until terminal kernel
// completion, so a dropped pending read still owns the socket. The kqueue and
// IOCP backends remove the operation synchronously at detach, where such
// incompatible extra ownership cannot arise.
#[cfg(target_os = "linux")]
#[karmaio::test]
async fn reunite_rejects_while_detached_read_is_in_flight() {
    let (listener, addr) = bind();

    // The server stays silent first, so the client's read remains pending.
    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        karmaio::time::sleep(std::time::Duration::from_millis(50)).await;
        socket.write_all(b"late".to_vec()).await.unwrap();
        socket.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, write_half) = stream.into_split();

    // Submit a read against the silent peer, then drop the future: the
    // operation stays in flight and keeps owning the socket.
    {
        let mut future = std::pin::pin!(read_half.read(Box::new([0u8; 64])));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }

    // A detached operation holds incompatible ownership: reunification fails
    // and returns both halves.
    let error = match <TcpStream as ReuniteOwned>::reunite(read_half, write_half) {
        Ok(_) => panic!("reunite must fail while a detached read owns the socket"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ReuniteErrorKind::NotQuiescent);
    let (read_half, mut write_half) = error.into_halves();

    // Once the peer completes the detached read and the driver processes the
    // completion, the same halves can be reunited.
    write_half.write_all(b"ping".to_vec()).await.unwrap();
    karmaio::time::sleep(std::time::Duration::from_millis(100)).await;

    let stream = <TcpStream as ReuniteOwned>::reunite(read_half, write_half).unwrap();
    stream.close().await.unwrap();
    server.await.unwrap();
}

#[cfg(unix)]
#[karmaio::test]
async fn descriptor_is_released_when_last_owner_disappears() {
    use std::os::fd::AsRawFd;

    let (listener, addr) = bind();

    // One accept per original connection plus one per candidate below.
    let server = spawn_local(async move {
        for _ in 0..64 {
            let (socket, _) = listener.accept().await.unwrap();
            socket.close().await.unwrap();
        }
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let freed_fd = stream.as_raw_fd();

    let (read_half, write_half) = stream.into_split();
    drop(read_half);
    drop(write_half);

    // The descriptor must be closed exactly once: new sockets reuse the freed
    // number because the kernel allocates the lowest available fd.
    // Candidates are kept alive so each connect allocates a fresh fd; the
    // freed fd must be allocated again once it is the lowest free one.
    let mut reused = false;
    let mut candidates = Vec::new();
    for _ in 0..64 {
        match TcpStream::connect(addr).await {
            Ok(candidate) => {
                if candidate.as_raw_fd() == freed_fd {
                    reused = true;
                }
                candidates.push(candidate);
            }
            // The server may have just drained the last pending connection and
            // dropped the listener; that is only acceptable once the freed fd
            // has already been observed to be reused.
            Err(e) => {
                assert!(reused, "fd {freed_fd} was never reused before the listener closed: {e}");
                break;
            }
        }
    }
    assert!(
        reused,
        "fd {freed_fd} was never reused after the halves dropped: n={}",
        candidates.len()
    );

    server.await.unwrap();
}
