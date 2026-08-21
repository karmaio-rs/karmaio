#![cfg(unix)]

//! Eager cancellation for Unix stream one-shot reads and writes.

use std::{os::unix::net::SocketAddr as UnixSocketAddr, path::PathBuf, time::Duration};

use karmaio::io::{AsyncReadCancellable, AsyncReadExt, AsyncWriteExt, Canceller, is_operation_canceled};
use karmaio::net::unix::{UnixListener, UnixStream};
use karmaio::runtime::spawn_local;
use karmaio::time::sleep;

fn bind() -> (UnixListener, UnixSocketAddr) {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "karmaio-cancel-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[karmaio::test]
async fn cancel_pending_unix_read_on_silent_peer() {
    let (listener, addr) = bind();
    let path = addr.as_pathname().unwrap().to_owned();
    let _server = spawn_local(async move {
        let _accepted = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let mut client = UnixStream::connect(&path).await.unwrap();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });

    let buf = vec![0u8; 16];
    let original = buf.as_ptr();
    let (res, buf) = client.read_cancellable(buf, &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");
    assert_eq!(buf.as_ptr(), original);
}

#[karmaio::test]
async fn canceled_unix_read_leaves_write_half_usable() {
    let (listener, addr) = bind();
    let path = addr.as_pathname().unwrap().to_owned();
    let server = spawn_local(async move {
        let mut socket = listener.accept().await.unwrap();
        let (res, buf) = socket.read_exact(vec![0u8; 4]).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"pong");
    });

    let stream = UnixStream::connect(&path).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();
    let canceller = Canceller::new();
    let handle = canceller.handle();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });
    let (res, _) = read_half.read_cancellable(vec![0u8; 8], &handle).await.into_parts();
    assert!(is_operation_canceled(res.as_ref().unwrap_err()), "{res:?}");
    write_half.write_all(b"pong".to_vec()).await.unwrap();
    server.await.unwrap();
}
