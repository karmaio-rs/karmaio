//! A simple UDP client that sends a message and waits for an echo.
//!
//! ```text
//! # terminal 1
//! cargo run --example udp_echo
//!
//! # terminal 2
//! cargo run --example udp_client
//! ```

use std::net::SocketAddr;

use karmaio::net::udp::UdpSocket;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).await?;
    let local_addr = socket.local_addr()?;
    println!("bound to: {local_addr}");

    let server_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    socket.connect(server_addr).await?;
    println!("connected to: {server_addr}");

    let (result, _) = socket.send(b"Hello UDP!".to_vec()).await.into_parts();
    result?;
    println!("sent message");

    let buf = vec![0; 1024];
    let (result, buf) = socket.recv(buf).await.into_parts();
    let n = result?;
    println!("received {n} bytes: {}", String::from_utf8_lossy(&buf[..n]));

    Ok(())
}
