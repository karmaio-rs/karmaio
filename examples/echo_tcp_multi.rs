//! TCP echo server using `incoming()` (Linux multishot accept under the hood).
//!
//! Same behaviour as `echo_tcp`, but connections are accepted via
//! [`TcpListener::incoming`](karmaio::net::tcp::TcpListener::incoming)
//! instead of a oneshot accept loop. On Linux this uses io_uring multishot
//! accept so the kernel can post completions without a userspace resubmit
//! between accepts.
//!
//! Requires **Linux 6.12+**. The multishot request is not auto-rearmed after
//! it ends; this example treats stream end as server exit.
//!
//! ```text
//! # terminal 1 (Linux)
//! cargo run -p karmaio-examples --example echo_tcp_multi
//!
//! # terminal 2
//! cargo run -p karmaio-examples --example hello_world
//! ```

#[cfg(target_os = "linux")]
use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use karmaio::io::{AsyncRead, AsyncWriteExt, Stream};
#[cfg(target_os = "linux")]
use karmaio::net::tcp::TcpListener;
#[cfg(target_os = "linux")]
use karmaio::runtime::spawn_local;

#[cfg(target_os = "linux")]
#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("Listening on: {addr} (incoming / multishot accept)");

    let mut incoming = listener.incoming()?;

    while let Some(item) = incoming.next().await {
        let (mut socket, peer) = item?;
        println!("accepted connection from {peer}");

        // Detach the handler: dropping the JoinHandle does not abort the task.
        spawn_local(async move {
            let mut buf = vec![0; 4096];

            loop {
                let (result, returned_buf) = socket.read(buf).await.into_parts();
                buf = returned_buf;

                match result {
                    Ok(0) => {
                        // Peer closed the connection.
                        return;
                    }
                    Ok(_n) => {
                        let (write_result, returned_buf) = socket.write_all(buf).await.into_parts();
                        buf = returned_buf;
                        if let Err(e) = write_result {
                            eprintln!("failed to write to {peer}: {e}");
                            return;
                        }
                        buf.clear();
                    }
                    Err(e) => {
                        eprintln!("failed to read from {peer}: {e}");
                        return;
                    }
                }
            }
        });
    }

    // Multishot request ended (error or kernel disarmed). No auto-rearm.
    println!("incoming stream ended");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("echo_tcp_multi requires Linux (io_uring multishot accept, kernel 6.12+).");
    std::process::exit(1);
}
