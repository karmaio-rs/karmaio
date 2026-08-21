//! Eager cancellation for UDP datagram reads and writes.

use std::{net::SocketAddr, time::Duration};

use karmaio::io::{Canceller, is_operation_canceled};
use karmaio::net::udp::UdpSocket;
use karmaio::runtime::spawn_local;
use karmaio::time::sleep;

async fn bind() -> (UdpSocket, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

#[karmaio::test]
async fn cancel_before_submit_returns_buffer() {
    let (_quiet, _) = bind().await;
    let socket = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    canceller.cancel();

    let buf = vec![0u8; 16];
    let original = buf.as_ptr();
    let (res, buf) = socket.recv_from_cancellable(buf, &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
    assert_eq!(buf.as_ptr(), original);
    assert_eq!(buf.len(), 16);
}

#[karmaio::test]
async fn cancel_pending_recv_from_on_quiet_socket_returns_buffer() {
    // No peer ever sends to this socket, so the only way the read completes
    // is through cancellation.
    let (_quiet, _) = bind().await;
    let socket = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });

    let buf = vec![0u8; 32];
    let original = buf.as_ptr();
    let (res, buf) = socket.recv_from_cancellable(buf, &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");
    assert_eq!(buf.as_ptr(), original);
    assert_eq!(buf.len(), 32);
}

#[karmaio::test]
async fn cancel_is_idempotent_and_sticky() {
    let (_quiet, _) = bind().await;
    let socket = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    canceller.cancel();
    canceller.cancel();

    let (res, buf) = socket.recv_from_cancellable(vec![0u8; 8], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
    assert_eq!(buf.len(), 8);

    let (res, _) = socket.recv_from_cancellable(vec![0u8; 8], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
}

#[karmaio::test]
async fn canceled_recv_leaves_send_to_usable() {
    let (server, addr) = bind().await;
    let server_task = spawn_local(async move {
        let buf = vec![0u8; 4];
        let (res, buf) = server.recv_from(buf).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"ping");
    });

    let client = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });
    let (res, _) = client.recv_from_cancellable(vec![0u8; 16], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");

    let (res, _) = client.send_to(b"ping".to_vec(), addr).await.into_parts();
    res.unwrap();
    server_task.await.unwrap();
}
