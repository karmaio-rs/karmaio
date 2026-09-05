//! TCP loopback round-trip for length-delimited framed I/O.

#![cfg(all(feature = "macros", feature = "net"))]

use std::net::SocketAddr;

use karmaio::io::{BytesCodec, Framed, LengthDelimited, Sink, Stream};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::spawn_local;

#[karmaio::test]
async fn length_delimited_tcp_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();

    let server = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut framed = Framed::with_duplex(socket, BytesCodec::new(), LengthDelimited::new());

        let first = framed.next().await.unwrap().unwrap();
        assert_eq!(first, b"hello");
        let second = framed.next().await.unwrap().unwrap();
        assert_eq!(second, b"world");

        framed.send(b"ack".to_vec()).await.unwrap();
        framed.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut framed = Framed::with_duplex(stream, BytesCodec::new(), LengthDelimited::new());
    framed.send(b"hello".to_vec()).await.unwrap();
    framed.send(b"world".to_vec()).await.unwrap();
    framed.flush().await.unwrap();

    let ack = framed.next().await.unwrap().unwrap();
    assert_eq!(ack, b"ack");

    // Server task should finish cleanly.
    server.await.unwrap();
}
