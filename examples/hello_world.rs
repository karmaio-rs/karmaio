//! A simple client that opens a TCP stream, writes "hello world\n", and closes
//! the connection.
//!
//! To start a server that this client can talk to on port 6142, you can use this command:
//!
//!     ncat -l 6142
//!
//! And then in another terminal run:
//!
//!     cargo run --example hello_world

use std::net::SocketAddr;

use karmaio::io::AsyncWriteExt;
use karmaio::net::tcp::TcpStream;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:6142".parse().unwrap();
    let mut stream = TcpStream::connect(addr).await?;
    println!("created stream");

    let (result, _) = stream.write_all(b"hello world\n".to_vec()).await;
    println!("wrote to stream; success={:?}", result.is_ok());

    stream.close().await
}