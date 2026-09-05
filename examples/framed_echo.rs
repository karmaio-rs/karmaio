//! Length-delimited framed TCP echo server.
//!
//! Each message is a big-endian `u32` length prefix followed by the payload.
//! The server reads frames and writes them back unchanged.
//!
//! ```text
//! # terminal 1
//! cargo run -p karmaio-examples --example framed_echo
//!
//! # terminal 2 — send two frames with a tiny Python client, or use any
//! # length-prefixed client compatible with `LengthDelimited`.
//! ```

use std::net::SocketAddr;

use karmaio::io::{BytesCodec, Framed, LengthDelimited, Sink, Stream};
use karmaio::net::tcp::TcpListener;
use karmaio::runtime::spawn_local;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("framed echo listening on {addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        println!("accepted connection from {peer}");

        spawn_local(async move {
            let mut framed = Framed::with_duplex(socket, BytesCodec::new(), LengthDelimited::new());

            while let Some(item) = framed.next().await {
                match item {
                    Ok(payload) => {
                        if let Err(e) = framed.send(payload).await {
                            eprintln!("write to {peer} failed: {e}");
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("read from {peer} failed: {e}");
                        return;
                    }
                }
            }

            let _ = framed.close().await;
            println!("connection from {peer} closed");
        });
    }
}
