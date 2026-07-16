//! A UDP echo server.
//!
//! Listens on `127.0.0.1:8080` and sends every datagram back to its source.
//! Pair with the client:
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
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let socket = UdpSocket::bind(addr).await?;
    println!("UDP echo listening on {addr}");

    let mut buf = vec![0u8; 1024];
    loop {
        let (result, returned) = socket.recv_from(buf).await;
        buf = returned;
        let (n, peer) = result?;
        println!("received {n} bytes from {peer}");

        let payload = buf[..n].to_vec();
        let (send_result, _) = socket.send_to(payload, peer).await;
        send_result?;
    }
}
