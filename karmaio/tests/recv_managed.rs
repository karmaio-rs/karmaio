//! Managed oneshot receive tests (Linux only).
//!
//! Requires Linux 6.12+. The suite does not probe the kernel version.

#![cfg(all(target_os = "linux", feature = "net", feature = "macros"))]

use std::io::Write;
use std::net::SocketAddr;
use std::thread;

use karmaio::io::AsyncReadManaged;
use karmaio::net::tcp::TcpListener;
use karmaio::net::udp::UdpSocket;

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
