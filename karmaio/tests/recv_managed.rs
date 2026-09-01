//! Managed oneshot receive tests (Linux only).
//!
//! Requires Linux 6.12+. The suite does not probe the kernel version.

#![cfg(all(target_os = "linux", feature = "net", feature = "macros"))]

use std::{io::Write, net::SocketAddr, thread, time::Duration};

use karmaio::io::AsyncReadManaged;
use karmaio::net::tcp::TcpListener;
use karmaio::net::udp::UdpSocket;
use karmaio::runtime::{CancellationSource, FutureExt, is_operation_canceled, spawn_local};
use karmaio::time::sleep;

#[karmaio::test]
async fn tcp_recv_managed_reads_payload() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.write_all(b"hello").expect("write");
        stream
    });

    let (stream, _) = listener.accept().await.expect("accept");
    let buf = stream.recv_managed(0).await.expect("recv_managed").expect("payload");
    assert_eq!(&buf[..], b"hello");
    buf.release();

    // Trait path
    let client2 = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.write_all(b"world").expect("write");
    });
    let (mut stream2, _) = listener.accept().await.expect("accept");
    let buf = stream2.read_managed(0).await.expect("read_managed").expect("payload");
    assert_eq!(&buf[..], b"world");

    drop(client.join().expect("client"));
    client2.join().expect("client2");
}

#[karmaio::test(buffer_pool_size = 1, buffer_pool_buffer_len = 1)]
async fn owned_read_half_returns_exact_ranges_and_recycles_leases() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.write_all(b"ab").expect("write");
    });

    let (stream, _) = listener.accept().await.expect("accept");
    let (mut read_half, write_half) = stream.into_split();
    let first = read_half
        .read_managed(0)
        .await
        .expect("first read")
        .expect("first byte");
    assert_eq!(&first[..], b"a");
    first.release();

    let second = read_half
        .read_managed(0)
        .await
        .expect("second read")
        .expect("second byte");
    assert_eq!(&second[..], b"b");
    drop(second);
    assert!(read_half.read_managed(0).await.expect("EOF read").is_none());

    drop(read_half);
    drop(write_half);
    client.join().expect("client");
}

#[karmaio::test]
async fn owned_read_half_managed_read_observes_cancellation() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || std::net::TcpStream::connect(addr).expect("connect"));

    let (stream, _) = listener.accept().await.expect("accept");
    let (mut read_half, write_half) = stream.into_split();
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let error = read_half
        .read_managed(0)
        .with_cancellation(token)
        .await
        .expect_err("managed read should be canceled");
    assert!(is_operation_canceled(&error), "{error:?}");

    drop(read_half);
    drop(write_half);
    drop(client.join().expect("client"));
}

#[karmaio::test]
async fn owned_read_half_managed_read_reports_connection_error() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let stream = std::net::TcpStream::connect(addr).expect("connect");
        socket2::SockRef::from(&stream)
            .set_linger(Some(Duration::ZERO))
            .expect("set linger");
    });

    let (stream, _) = listener.accept().await.expect("accept");
    let (mut read_half, write_half) = stream.into_split();
    client.join().expect("client");
    let error = read_half.read_managed(0).await.expect_err("reset should be reported");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    ));

    drop(read_half);
    drop(write_half);
}

#[karmaio::test]
async fn connected_udp_recv_managed() {
    let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind client");
    client.connect(server_addr).await.expect("connect");
    let client_addr = client.local_addr().unwrap();
    server.connect(client_addr).await.expect("server connect");

    client.send(b"ping".to_vec()).await.0.expect("send");

    let datagram = server.recv_managed(0).await.expect("recv_managed");
    assert_eq!(&datagram.buffer[..], b"ping");
    assert_eq!(datagram.original_len, 4);
    assert!(datagram.peer.is_none());
}

#[karmaio::test]
async fn connected_udp_recv_managed_preserves_empty_datagram() {
    let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind client");
    client.connect(server_addr).await.expect("connect");
    let client_addr = client.local_addr().unwrap();
    server.connect(client_addr).await.expect("server connect");

    client.send(Vec::new()).await.0.expect("send empty datagram");

    let datagram = server.recv_managed(0).await.expect("recv_managed");
    assert!(datagram.buffer.is_empty());
    assert_eq!(datagram.original_len, 0);
}

#[karmaio::test]
async fn connected_udp_recv_managed_reports_truncation() {
    let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind client");
    client.connect(server_addr).await.expect("connect");
    let client_addr = client.local_addr().unwrap();
    server.connect(client_addr).await.expect("server connect");

    client.send(vec![7; 32]).await.0.expect("send datagram");

    let datagram = server.recv_managed(4).await.expect("recv_managed");
    assert_eq!(datagram.buffer.len(), 4);
    assert_eq!(datagram.original_len, 32);
    assert!(datagram.is_truncated());
}
