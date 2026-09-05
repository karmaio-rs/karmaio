//! Integration coverage for `write_vectored_all`.

#![cfg(all(feature = "macros", feature = "net"))]

use std::{net::SocketAddr, time::Duration};

use karmaio::io::{AsyncReadExt, AsyncWriteExt};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::{CancellationSource, FutureExt, is_operation_canceled, spawn_local};
use karmaio::time::sleep;

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[karmaio::test]
async fn tcp_write_vectored_all_delivers_every_component() {
    let (listener, addr) = bind();
    let server = spawn_local(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (res, buf) = socket.read_exact(vec![0u8; 11]).await.into_parts();
        res.unwrap();
        assert_eq!(&buf[..], b"hello world");
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let bufs = vec![b"hello".to_vec(), b" ".to_vec(), b"world".to_vec()];
    let (result, returned) = client.write_vectored_all(bufs).await.into_parts();

    result.unwrap();
    assert_eq!(returned, [b"hello".to_vec(), b" ".to_vec(), b"world".to_vec()]);
    server.await.unwrap();
}

#[karmaio::test]
async fn tcp_write_vectored_all_cancels_on_silent_peer() {
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

    // Fill the socket buffer so a vectored write can remain pending.
    let chunk = vec![0u8; 256 * 1024];
    let bufs = vec![chunk.clone(), chunk.clone(), chunk.clone(), chunk];
    let total = 1024 * 1024;
    let (res, buf) = client
        .write_vectored_all(bufs)
        .with_cancellation(token)
        .await
        .into_parts();
    match res {
        Ok(written) => assert_eq!(written, total),
        Err(err) => {
            assert!(is_operation_canceled(&err), "{err:?}");
            assert_eq!(buf.len(), 4);
            assert_eq!(buf[0].len(), 256 * 1024);
        }
    }
}

#[cfg(unix)]
mod unix_tests {
    use std::path::PathBuf;

    use karmaio::io::{AsyncReadExt, AsyncWriteExt};
    use karmaio::net::unix::{UnixListener, UnixStream};
    use karmaio::runtime::spawn_local;

    fn bind() -> (UnixListener, PathBuf) {
        let path: PathBuf = std::env::temp_dir().join(format!(
            "karmaio-write-vectored-all-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        (listener, path)
    }

    #[karmaio::test]
    async fn unix_write_vectored_all_delivers_every_component() {
        let (listener, path) = bind();
        let server = spawn_local(async move {
            let mut socket = listener.accept().await.unwrap();
            let (res, buf) = socket.read_exact(vec![0u8; 8]).await.into_parts();
            res.unwrap();
            assert_eq!(&buf[..], b"abcd1234");
        });

        let mut client = UnixStream::connect(&path).await.unwrap();
        let bufs = [*b"abcd", *b"1234"];
        let (result, returned) = client.write_vectored_all(bufs).await.into_parts();

        result.unwrap();
        assert_eq!(returned, [*b"abcd", *b"1234"]);
        server.await.unwrap();
    }
}
