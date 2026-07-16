//! A simple TCP client that connects, writes a line, and closes.
//!
//! Pair this with the echo server:
//!
//! ```text
//! # terminal 1
//! cargo run --example echo_tcp
//!
//! # terminal 2
//! cargo run --example hello_world
//! ```

use std::net::SocketAddr;

use karmaio::io::AsyncWriteExt;
use karmaio::net::tcp::TcpStream;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut stream = TcpStream::connect(addr).await?;
    println!("connected to {addr}");

    let (result, _) = stream.write_all(b"hello world\n".to_vec()).await;
    println!("wrote to stream; success={:?}", result.is_ok());

    stream.close().await
}
