//! Incoming-connection stream tests (Linux only; multishot accept under the hood).
//!
//! Requires Linux 6.12+. The suite does not probe the kernel version.

#![cfg(all(target_os = "linux", feature = "net", feature = "macros"))]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::thread;

use karmaio::io::{AsyncReadExt, AsyncWriteExt, Stream};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::net::unix::UnixListener;
use karmaio::runtime::spawn_local;

#[karmaio::test]
async fn tcp_incoming_accepts_multiple_clients() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let clients = thread::spawn(move || {
        let mut streams = Vec::new();
        for i in 0..4u8 {
            let mut stream = std::net::TcpStream::connect(addr).expect("connect");
            stream.write_all(&[i]).expect("write");
            streams.push(stream);
        }
        streams
    });

    let mut incoming = listener.incoming().expect("incoming");
    let mut got = Vec::new();
    for _ in 0..4 {
        let (mut stream, peer) = incoming.next().await.expect("item").expect("accept ok");
        assert_eq!(peer.ip().to_string(), "127.0.0.1");
        let (res, buf) = stream.read_exact(vec![0u8; 1]).await.into_parts();
        res.expect("read");
        got.push(buf[0]);
        stream.write_all(b"ok".to_vec()).await.0.expect("write");
    }

    got.sort_unstable();
    assert_eq!(got, vec![0, 1, 2, 3]);

    let mut client_streams = clients.join().expect("client thread");
    for stream in &mut client_streams {
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).expect("client read");
        assert_eq!(&buf, b"ok");
    }
}

#[karmaio::test]
async fn unix_incoming_accepts_multiple_clients() {
    let dir = std::env::temp_dir();
    let path: PathBuf = dir.join(format!("karmaio-incoming-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path).expect("bind unix");
    let client_path = path.clone();

    let clients = thread::spawn(move || {
        let mut streams = Vec::new();
        for i in 0..3u8 {
            let mut stream = StdUnixStream::connect(&client_path).expect("connect unix");
            stream.write_all(&[i]).expect("write");
            streams.push(stream);
        }
        streams
    });

    let mut incoming = listener.incoming().expect("incoming");
    let mut got = Vec::new();
    for _ in 0..3 {
        let mut stream = incoming.next().await.expect("item").expect("accept ok");
        let (res, buf) = stream.read_exact(vec![0u8; 1]).await.into_parts();
        res.expect("read");
        got.push(buf[0]);
    }

    got.sort_unstable();
    assert_eq!(got, vec![0, 1, 2]);
    let _ = clients.join().expect("client thread");
    let _ = std::fs::remove_file(&path);
}

#[karmaio::test]
async fn tcp_incoming_drop_cancels_and_oneshot_still_works() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    {
        let mut incoming = listener.incoming().expect("incoming");
        let connector = thread::spawn(move || std::net::TcpStream::connect(addr).expect("connect"));

        let (stream, _) = incoming.next().await.expect("item").expect("accept ok");
        drop(stream);
        drop(incoming);
        let _ = connector.join().expect("connector");
    }

    // After cancelling the incoming stream, oneshot accept should still work.
    let connector = thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect after cancel");
        stream.write_all(b"x").expect("write");
        stream
    });

    let (mut stream, _) = listener.accept().await.expect("oneshot accept");
    let (res, buf) = stream.read_exact(vec![0u8; 1]).await.into_parts();
    res.expect("read");
    assert_eq!(buf[0], b'x');
    let _ = connector.join().expect("connector");
}

#[karmaio::test]
async fn tcp_incoming_with_spawned_handlers() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let server = spawn_local(async move {
        let mut incoming = listener.incoming().expect("incoming");
        for expected in 0..2u8 {
            let (mut stream, _) = incoming.next().await.expect("item").expect("accept");
            let (res, buf) = stream.read_exact(vec![0u8; 1]).await.into_parts();
            res.expect("read");
            assert_eq!(buf[0], expected);
            stream.write_all(vec![expected + 10]).await.0.expect("echo");
        }
    });

    for i in 0..2u8 {
        let mut client = TcpStream::connect(addr).await.expect("connect");
        client.write_all(vec![i]).await.0.expect("write");
        let (res, buf) = client.read_exact(vec![0u8; 1]).await.into_parts();
        res.expect("read");
        assert_eq!(buf[0], i + 10);
    }

    server.await.expect("server task");
}
