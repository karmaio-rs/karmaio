//! Managed / multishot UDP recv_from tests (Linux only).
//!
//! Requires Linux 6.12+. The suite does not probe the kernel version.

#![cfg(all(target_os = "linux", feature = "net", feature = "macros"))]

use std::net::SocketAddr;

use karmaio::io::Stream;
use karmaio::net::udp::UdpSocket;

#[karmaio::test]
async fn udp_recv_from_managed_returns_peer() {
    let server = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind client");
    let client_addr = client.local_addr().unwrap();

    client.send_to(b"hello".to_vec(), server_addr).await.0.expect("send_to");

    let datagram = server.recv_from_managed(0).await.expect("recv_from_managed");
    assert_eq!(&datagram.buffer[..], b"hello");
    assert_eq!(datagram.peer, Some(client_addr));
}

#[karmaio::test]
async fn udp_recv_from_multi_reads_datagrams() {
    let server = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind client");
    let client_addr = client.local_addr().unwrap();

    for i in 0..3u8 {
        client.send_to(vec![i; 3], server_addr).await.0.expect("send_to");
    }

    let mut stream = server.recv_from_multi().expect("recv_from_multi");
    let mut got = Vec::new();
    while got.len() < 3 {
        let datagram = stream.next().await.expect("item").expect("datagram ok");
        assert_eq!(datagram.peer, Some(client_addr));
        got.push(datagram.buffer[0]);
        datagram.buffer.release();
    }
    got.sort_unstable();
    assert_eq!(got, vec![0, 1, 2]);
}

#[karmaio::test]
async fn udp_recv_from_managed_preserves_empty_datagram() {
    let server = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind client");
    let client_addr = client.local_addr().unwrap();
    client
        .send_to(Vec::new(), server_addr)
        .await
        .0
        .expect("send empty datagram");

    let datagram = server.recv_from_managed(0).await.expect("recv_from_managed");
    assert!(datagram.buffer.is_empty());
    assert_eq!(datagram.peer, Some(client_addr));
}

#[karmaio::test]
async fn udp_recv_from_multi_preserves_empty_datagram() {
    let server = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind client");
    let client_addr = client.local_addr().unwrap();
    client
        .send_to(Vec::new(), server_addr)
        .await
        .0
        .expect("send empty datagram");

    let mut recv = server.recv_from_multi().expect("recv_from_multi");
    let datagram = recv.next().await.expect("stream item").expect("datagram");
    assert!(datagram.buffer.is_empty());
    assert_eq!(datagram.peer, Some(client_addr));
}

#[karmaio::test]
async fn udp_recv_from_managed_reports_truncation() {
    let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();
    let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind client");

    client.send_to(vec![3; 32], server_addr).await.0.expect("send_to");
    let datagram = server.recv_from_managed(4).await.expect("recv_from_managed");
    assert_eq!(datagram.buffer.len(), 4);
    assert_eq!(datagram.original_len, 32);
    assert!(datagram.flags.is_truncated());
}

#[karmaio::test(buffer_pool_buffer_len = 192)]
async fn udp_recv_from_multi_reports_truncation() {
    let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind server");
    let server_addr = server.local_addr().unwrap();
    let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind client");

    client.send_to(vec![9; 512], server_addr).await.0.expect("send_to");
    let mut recv = server.recv_from_multi().expect("recv_from_multi");
    let datagram = recv.next().await.expect("stream item").expect("datagram");
    assert!(datagram.buffer.len() < datagram.original_len);
    assert_eq!(datagram.original_len, 512);
    assert!(datagram.is_truncated());
}
