//! Demonstrates async signal handling with karmaio.
//!
//! This example shows how to:
//! - Handle Ctrl-C signals
//! - Handle Unix signals (SIGTERM, SIGHUP, etc.)
//!
//! Run the example and send signals:
//!
//!     cargo run --example signal
//!
//! Then in another terminal:
//!
//!     kill -SIGTERM <pid>
//!     kill -SIGHUP <pid>
//!
//! Or press Ctrl-C in the first terminal.

#[cfg(unix)]
use karmaio::signal::{SignalKind, signal, ctrl_c};

#[cfg(windows)]
use karmaio::signal::ctrl_c;

use std::time::Duration;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    println!("Signal handling example started.");
    println!("Press Ctrl-C to test Ctrl-C handling.");

    // Example 1: Ctrl-C handling
    println!("\nExample 1: Ctrl-C handling");
    let ctrl_c_future = ctrl_c()?;
    println!("Waiting for Ctrl-C...");

    // We'll wait for Ctrl-C with a timeout
    match karmaio::time::timeout(Duration::from_secs(5), ctrl_c_future).await {
        Ok(_) => println!("Received Ctrl-C!"),
        Err(_) => println!("No Ctrl-C received within 5 seconds (timeout)."),
    }

    // Example 2: Unix signals (only on Unix)
    #[cfg(unix)]
    {
        println!("\nExample 2: Unix signal handling");
        let mut sigterm = signal(SignalKind::terminate())?;
        let _sighup = signal(SignalKind::hangup())?;

        println!("Waiting for SIGTERM or SIGHUP...");
        println!("Try: kill -SIGTERM {}", std::process::id());
        println!("Or:  kill -SIGHUP {}", std::process::id());

        // Wait for SIGTERM with a timeout
        match karmaio::time::timeout(Duration::from_secs(10), sigterm.recv()).await {
            Ok(_) => println!("Received SIGTERM!"),
            Err(_) => println!("No SIGTERM received within 10 seconds (timeout)."),
        }
    }

    println!("\nSignal handling example completed!");
    Ok(())
}