//! A "hello world" echo server with karmaio
//!
//! This server will create a TCP listener, accept connections in a loop, and
//! write back everything that's read off of each TCP connection.
//!
//! Because karmaio is a thread-per-core runtime, each TCP connection is
//! processed concurrently on the same thread.
//!
//! To see this server in action, you can run this in one terminal:
//!
//!     cargo run --example echo_tcp
//!
//! and in another terminal you can run:
//!
//!     cargo run --example hello_world
//!
//! Each line you type in to the `hello_world` terminal should be echoed back to
//! you!

use std::net::SocketAddr;

use karmaio::io::{AsyncRead, AsyncWriteExt};
use karmaio::net::tcp::TcpListener;
use karmaio::runtime::local::spawn_local;

const DEFAULT_ADDR: &str = "127.0.0.1:8080";
const BUFFER_SIZE: usize = 4096;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = DEFAULT_ADDR.parse().unwrap();
    let listener = TcpListener::bind(addr)?;
    println!("Listening on: {addr}");

    loop {
        let (mut socket, addr) = listener.accept().await?;
        println!("accepted a connection from {addr}");

        spawn_local(async move {
            let mut buf = vec![0; BUFFER_SIZE];

            loop {
                // Read data from the socket
                let (result, returned_buf) = socket.read(buf).await;
                buf = returned_buf;
                
                match result {
                    Ok(0) => {
                        // Connection closed by peer
                        return;
                    }
                    Ok(_n) => {
                        // Write the data back. If writing fails, log the error and exit.
                        let (write_result, returned_buf) = socket.write_all(buf).await;
                        buf = returned_buf;
                        if let Err(e) = write_result {
                            eprintln!("Failed to write to socket {}: {}", addr, e);
                            return;
                        }
                        // Clear the buffer for the next read
                        buf.clear();
                    }
                    Err(e) => {
                        eprintln!("Failed to read from socket {}: {}", addr, e);
                        return;
                    }
                }
            }
        });
    }
}