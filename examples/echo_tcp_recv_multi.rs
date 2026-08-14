//! TCP echo using multishot accept + multishot managed recv (Linux).
//!
//! - Accepts with [`TcpListener::incoming`](karmaio::net::tcp::TcpListener::incoming)
//! - Reads with [`TcpStream::recv_multi`](karmaio::net::tcp::TcpStream::recv_multi)
//!   into runtime pool buffers ([`PooledBuf`](karmaio::buf::PooledBuf))
//!
//! # Buffer leases
//!
//! Each `recv_multi` item is a **lease** on a pool buffer. This example releases
//! the lease after echoing. Holding many leases without recycle can exhaust the
//! pool and end the stream with `ENOBUFS`.
//!
//! Requires **Linux 6.12+**. Multishot requests are **not** auto-rearmed.
//!
//! ```text
//! # terminal 1 (Linux)
//! cargo run -p karmaio-examples --example echo_tcp_recv_multi
//!
//! # terminal 2
//! cargo run -p karmaio-examples --example hello_world
//! ```

#[cfg(target_os = "linux")]
use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use karmaio::buf::IoBuf;
#[cfg(target_os = "linux")]
use karmaio::io::{AsyncWriteExt, Stream};
#[cfg(target_os = "linux")]
use karmaio::net::tcp::TcpListener;
#[cfg(target_os = "linux")]
use karmaio::runtime::spawn_local;

#[cfg(target_os = "linux")]
#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("Listening on: {addr} (incoming + recv_multi)");

    let mut incoming = listener.incoming()?;

    while let Some(item) = incoming.next().await {
        let (socket, peer) = item?;
        println!("accepted connection from {peer}");

        spawn_local(async move {
            let mut recv = match socket.recv_multi() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("recv_multi failed for {peer}: {e}");
                    return;
                }
            };

            while let Some(item) = recv.next().await {
                match item {
                    Ok(buf) => {
                        let payload = buf.as_init().to_vec();
                        // Recycle the pool lease before awaiting write.
                        buf.release();
                        let (write_result, _) = (&socket).write_all(payload).await.into_parts();
                        if let Err(e) = write_result {
                            eprintln!("failed to write to {peer}: {e}");
                            return;
                        }
                    }
                    Err(e) => {
                        // Includes ENOBUFS when the pool is exhausted.
                        eprintln!("recv_multi error from {peer}: {e}");
                        return;
                    }
                }
            }
        });
    }

    println!("incoming stream ended");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("echo_tcp_recv_multi requires Linux (io_uring multishot accept/recv, kernel 6.12+).");
    std::process::exit(1);
}
