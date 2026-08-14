//! Multishot receive tests (Linux only).
//!
//! Requires Linux 6.12+. The suite does not probe the kernel version.

#![cfg(all(target_os = "linux", feature = "net", feature = "macros"))]

use std::io::Write;
use std::net::SocketAddr;
use std::thread;

use karmaio::io::{AsyncReadMulti, Stream};
use karmaio::net::{tcp::TcpListener, udp::UdpSocket};

#[karmaio::test]
async fn tcp_recv_multi_reads_multiple_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        for i in 0..4u8 {
            stream.write_all(&[i; 4]).expect("write");
        }
        // Keep the socket open until the server has drained.
        stream
    });

    let (stream, _) = listener.accept().await.expect("accept");
    let mut recv = stream.recv_multi().expect("recv_multi");

    let mut got = Vec::new();
    while got.len() < 16 {
        let item = recv.next().await.expect("stream item");
        match item {
            Ok(buf) => {
                got.extend_from_slice(&buf);
                buf.release();
            }
            Err(err) => panic!("recv_multi error: {err}"),
        }
    }

    assert_eq!(got.len(), 16);
    assert_eq!(&got[0..4], &[0, 0, 0, 0]);
    assert_eq!(&got[12..16], &[3, 3, 3, 3]);

    drop(client.join().expect("client"));
}

#[karmaio::test]
async fn tcp_recv_multi_via_trait() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let _client = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.write_all(b"trait").expect("write");
        stream
    });

    let (mut stream, _) = listener.accept().await.expect("accept");
    let mut recv = stream.read_multi().expect("read_multi");
    let buf = loop {
        match recv.next().await {
            Some(Ok(buf)) => break buf,
            Some(Err(err)) => panic!("{err}"),
            None => panic!("stream ended early"),
        }
    };
    assert_eq!(&buf[..], b"trait");
}

#[karmaio::test]
async fn tcp_recv_multi_drop_cancels() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let client = thread::spawn(move || {
        let stream = std::net::TcpStream::connect(addr).expect("connect");
        // Idle connection; server drops the multishot stream.
        stream
    });

    let (stream, _) = listener.accept().await.expect("accept");
    let recv = stream.recv_multi().expect("recv_multi");
    drop(recv);

    // Stream still usable for oneshot managed recv after cancel.
    let mut client_stream = client.join().expect("client");
    client_stream.write_all(b"x").expect("write");
    let buf = stream.recv_managed(0).await.expect("managed").expect("byte");
    assert_eq!(&buf[..], b"x");
}

#[karmaio::test]
async fn tcp_recv_multi_ends_cleanly_on_eof() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let client = thread::spawn(move || std::net::TcpStream::connect(addr).expect("connect"));
    let (stream, _) = listener.accept().await.expect("accept");
    drop(client.join().expect("client"));

    let mut recv = stream.recv_multi().expect("recv_multi");
    assert!(recv.next().await.is_none());
    assert!(recv.next().await.is_none());
}

#[test]
fn recv_multi_ends_when_its_runtime_is_dropped() {
    let mut runtime = karmaio::RuntimeBuilder::new().build().expect("runtime");
    let mut recv = runtime.block_on(async {
        let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind socket");
        socket.recv_multi().expect("recv_multi")
    });
    drop(runtime);

    let mut polling_runtime = karmaio::RuntimeBuilder::new().build().expect("polling runtime");
    polling_runtime.block_on(async {
        assert!(recv.next().await.is_none());
        assert!(recv.next().await.is_none());
    });
}

#[karmaio::test]
async fn connected_udp_recv_multi_preserves_empty_datagram() {
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

    let mut recv = server.recv_multi().expect("recv_multi");
    let datagram = recv.next().await.expect("stream item").expect("datagram");
    assert!(datagram.buffer.is_empty());
    assert_eq!(datagram.original_len, 0);
}
