//! Eager cancellation for TCP one-shot reads and writes.

use std::{net::SocketAddr, time::Duration};

use karmaio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::{CancellationSource, FutureExt, is_operation_canceled, spawn_local};
use karmaio::time::{sleep, timeout};

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
    let source = CancellationSource::new();
    source.cancel();

    let buf = vec![0u8; 16];
    let original = buf.as_ptr();
    let (res, buf) = client.read(buf).with_cancellation(source.token()).await.into_parts();
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
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let buf = vec![0u8; 32];
    let original = buf.as_ptr();
    let (res, buf) = client.read(buf).with_cancellation(token).await.into_parts();
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
    let source = CancellationSource::new();
    source.cancel();
    source.cancel();

    let (res, buf) = client
        .read(vec![0u8; 8])
        .with_cancellation(source.token())
        .await
        .into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()));
    assert_eq!(buf.len(), 8);

    let (res, _) = client
        .read(vec![0u8; 8])
        .with_cancellation(source.token())
        .await
        .into_parts();
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

    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });
    let (res, _) = read_half
        .read(vec![0u8; 16])
        .with_cancellation(token)
        .await
        .into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");

    write_half.write_all(b"ping".to_vec()).await.unwrap();
    server.await.unwrap();
}

#[karmaio::test]
async fn dropping_unwrapped_read_requests_cancel_and_leaves_write_half_usable() {
    let (listener, addr) = bind();
    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (res, buf) = socket.read_exact(vec![0u8; 4]).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"drop");
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();

    let elapsed = timeout(Duration::from_millis(20), read_half.read(vec![0u8; 16])).await;
    assert!(elapsed.is_err());

    write_half.write_all(b"drop".to_vec()).await.unwrap();
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
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    // Fill the socket buffer so a write can remain pending.
    let buf = vec![0u8; 1024 * 1024];
    let (res, buf) = client.write(buf).with_cancellation(token).await.into_parts();
    match res {
        Ok(_) => {
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
        let source = CancellationSource::new();
        let token = source.token();
        spawn_local(async move {
            sleep(Duration::from_millis(5)).await;
            source.cancel();
        });
        let (res, buf) = client.read(vec![0u8; 8]).with_cancellation(token).await.into_parts();
        assert!(res.is_err());
        assert_eq!(buf.len(), 8);
    }
}

#[karmaio::test]
async fn one_source_cancels_read_and_write() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();
    let source = CancellationSource::new();
    let read_token = source.token();
    let write_token = source.token();

    let read_task = spawn_local(async move { read_half.read(vec![0u8; 16]).with_cancellation(read_token).await });
    let write_task = spawn_local(async move {
        write_half
            .write(vec![0u8; 1024 * 1024])
            .with_cancellation(write_token)
            .await
    });
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let (read_res, _) = read_task.await.unwrap().into_parts();
    let (write_res, buf) = write_task.await.unwrap().into_parts();
    assert!(is_operation_canceled(read_res.as_ref().unwrap_err()), "{read_res:?}");
    match write_res {
        Ok(_) => assert_eq!(buf.len(), 1024 * 1024),
        Err(err) => assert!(is_operation_canceled(&err), "{err:?}"),
    }
}

#[karmaio::test]
async fn nested_tokens_either_source_cancels() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let outer = CancellationSource::new();
    let inner = CancellationSource::new();
    let inner_token = inner.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        inner.cancel();
    });

    let (res, buf) = client
        .read(vec![0u8; 16])
        .with_cancellation(outer.token())
        .with_cancellation(inner_token)
        .await
        .into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");
    assert_eq!(buf.len(), 16);
}

#[karmaio::test]
async fn write_all_propagates_cancellation() {
    let (listener, addr) = bind();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let buf = vec![0u8; 1024 * 1024];
    let (res, buf) = client.write_all(buf).with_cancellation(token).await.into_parts();
    match res {
        Ok(_) => assert_eq!(buf.len(), 1024 * 1024),
        Err(err) => {
            assert!(is_operation_canceled(&err), "{err:?}");
            assert_eq!(buf.len(), 1024 * 1024);
        }
    }
}
