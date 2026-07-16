//! A simple UDP client that sends a message to a server and receives a response.
//!
//! To start a server that this client can talk to on port 8080, you can use this command:
//!
//!     cargo run --example udp_server
//!
//! And then in another terminal run:
//!
//!     cargo run --example udp_client

use std::net::SocketAddr;

use karmaio::net::udp::UdpSocket;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).await?;
    let local_addr = socket.local_addr()?;
    println!("Bound to: {local_addr}");

    let server_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    socket.connect(server_addr).await?;
    println!("Connected to: {server_addr}");

    let (result, _) = socket.send(b"Hello UDP!".to_vec()).await;
    println!("Sent message; success={:?}", result.is_ok());

    let buf = vec![0; 1024];
    let (result, buf) = socket.recv(buf).await;
    let n = result?;
    println!("Received {} bytes: {}", n, String::from_utf8_lossy(&buf[..n]));

    socket.shutdown(std::net::Shutdown::Both)
}