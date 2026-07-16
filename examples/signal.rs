//! Async signal handling (Ctrl-C and Unix signals).
//!
//! ```text
//! cargo run --example signal
//! ```
//!
//! In another terminal (Unix):
//!
//! ```text
//! kill -SIGTERM <pid>
//! kill -SIGHUP <pid>
//! ```
//!
//! Or press Ctrl-C in the first terminal.

#[cfg(unix)]
use karmaio::signal::{SignalKind, ctrl_c, signal};

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

    match karmaio::time::timeout(Duration::from_secs(5), ctrl_c_future).await {
        Ok(_) => println!("Received Ctrl-C!"),
        Err(_) => println!("No Ctrl-C received within 5 seconds (timeout)."),
    }

    // Example 2: Unix signals
    #[cfg(unix)]
    {
        println!("\nExample 2: Unix signal handling");
        let mut sigterm = signal(SignalKind::terminate())?;
        let _sighup = signal(SignalKind::hangup())?;

        println!("Waiting for SIGTERM or SIGHUP...");
        println!("Try: kill -SIGTERM {}", std::process::id());
        println!("Or:  kill -SIGHUP {}", std::process::id());

        match karmaio::time::timeout(Duration::from_secs(10), sigterm.recv()).await {
            Ok(_) => println!("Received SIGTERM!"),
            Err(_) => println!("No SIGTERM received within 10 seconds (timeout)."),
        }
    }

    println!("\nSignal handling example completed!");
    Ok(())
}
