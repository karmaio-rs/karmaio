//! Minimal `TcpListener::incoming` demo (Linux only).
//!
//! Binds a loopback TCP listener, starts an incoming stream, and prints each
//! peer address as connections arrive. Connect with any TCP client (for
//! example `nc 127.0.0.1 8081`).
//!
//! Requires **Linux 6.12+** (multishot accept under the hood). The stream is
//! not auto-rearmed after the multishot request ends.
//!
//! ```text
//! # terminal 1 (Linux)
//! cargo run -p karmaio-examples --example accept_multi
//!
//! # terminal 2
//! nc 127.0.0.1 8081
//! ```

#[cfg(target_os = "linux")]
use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use karmaio::io::Stream;
#[cfg(target_os = "linux")]
use karmaio::net::tcp::TcpListener;

#[cfg(target_os = "linux")]
#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("incoming on {addr}");
    println!("connect with: nc 127.0.0.1 8081");

    let mut incoming = listener.incoming()?;
    let mut count = 0u64;

    while let Some(item) = incoming.next().await {
        match item {
            Ok((stream, peer)) => {
                count += 1;
                println!("#{count} accepted {peer}");
                // Close immediately; this demo only exercises incoming().
                drop(stream);
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                // Stream ends after a hard error; no automatic rearm.
                break;
            }
        }
    }

    println!("incoming stream finished after {count} connection(s)");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("accept_multi requires Linux (TcpListener::incoming / multishot accept, kernel 6.12+).");
    std::process::exit(1);
}
