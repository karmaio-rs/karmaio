//! Eager cancellation for TCP one-shot reads and writes.

use std::{net::SocketAddr, time::Duration};

use karmaio::io::{
    AsyncReadCancellable, AsyncReadExt, AsyncWriteCancellable, AsyncWriteExt, Canceller, is_operation_canceled,
};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::spawn_local;
use karmaio::time::sleep;

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[karmaio::test]
async fn cancel_before_submit_returns_buffer() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _ = listener.accept().await;
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    canceller.cancel();

    let buf = vec![0u8; 16];
    let original = buf.as_ptr();
    let (res, buf) = client.read_cancellable(buf, &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
    assert_eq!(buf.as_ptr(), original);
    assert_eq!(buf.len(), 16);
}

#[karmaio::test]
async fn cancel_pending_read_on_silent_peer_returns_buffer() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });

    let buf = vec![0u8; 32];
    let original = buf.as_ptr();
    let (res, buf) = client.read_cancellable(buf, &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");
    assert_eq!(buf.as_ptr(), original);
    assert_eq!(buf.len(), 32);
}

#[karmaio::test]
async fn cancel_is_idempotent_and_sticky() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    canceller.cancel();
    canceller.cancel();

    let (res, buf) = client.read_cancellable(vec![0u8; 8], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
    assert_eq!(buf.len(), 8);

    let (res, _) = client.read_cancellable(vec![0u8; 8], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
}

#[karmaio::test]
async fn canceled_read_leaves_write_half_usable() {
    let (listener, addr) = bind();
    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (res, buf) = socket.read_exact(vec![0u8; 4]).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"ping");
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();

    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });
    let (res, _) = read_half.read_cancellable(vec![0u8; 16], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");

    write_half.write_all(b"ping".to_vec()).await.unwrap();
    server.await.unwrap();
}

#[karmaio::test]
async fn cancel_pending_write_on_silent_peer() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });

    // Fill the socket buffer so a write can remain pending.
    let buf = vec![0u8; 1024 * 1024];
    let (res, buf) = client.write_cancellable(buf, &handle).await.into_parts();
    match res {
        Ok(_) => {
            // Completion won the race; buffer still returned.
            assert_eq!(buf.len(), 1024 * 1024);
        }
        Err(err) => {
            assert!(is_operation_canceled(&err), "{err:?}");
            assert_eq!(buf.len(), 1024 * 1024);
        }
    }
}

#[karmaio::test]
async fn many_cancel_cycles_restore_capacity() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(60)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    for _ in 0..32 {
        let canceller = Canceller::new();
        let handle = canceller.handle();
        spawn_local(async move {
            sleep(Duration::from_millis(5)).await;
            canceller.cancel();
        });
        let (res, buf) = client.read_cancellable(vec![0u8; 8], &handle).await.into_parts();
        assert!(res.is_err());
        assert_eq!(buf.len(), 8);
    }
}
