//! A TCP echo server.
//!
//! Accepts connections and writes back everything read from each socket.
//! Connections are handled concurrently on the same thread via
//! [`spawn_local`](karmaio::runtime::spawn_local). Dropping the returned
//! [`JoinHandle`](karmaio::JoinHandle) **detaches** the task (it keeps
//! running); it does not cancel the connection handler.
//!
//! ```text
//! # terminal 1
//! cargo run --example echo_tcp
//!
//! # terminal 2
//! cargo run --example hello_world
//! ```

use std::net::SocketAddr;

use karmaio::io::{AsyncRead, AsyncWriteExt};
use karmaio::net::tcp::TcpListener;
use karmaio::runtime::spawn_local;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("Listening on: {addr}");

    loop {
        let (mut socket, peer) = listener.accept().await?;
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
}
