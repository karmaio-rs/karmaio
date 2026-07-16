//! Demonstrates async timer operations with karmaio.
//!
//! This example shows how to:
//! - Use `sleep` to pause execution
//! - Use `interval` to create recurring timers
//! - Use `timeout` to limit async operations
//!
//! Run the example:
//!
//!     cargo run --example timer

use std::time::Duration;

use karmaio::time::{interval, sleep, timeout};

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    // Example 1: Simple sleep
    println!("Example 1: Simple sleep");
    println!("Sleeping for 100ms...");
    sleep(Duration::from_millis(100)).await;
    println!("Awake!");

    // Example 2: Interval
    println!("\nExample 2: Interval");
    let mut interval = interval(Duration::from_millis(200));
    for i in 1..=3 {
        interval.tick().await;
        println!("Tick {} at {:?}", i, std::time::Instant::now());
    }

    // Example 3: Timeout
    println!("\nExample 3: Timeout");
    let result = timeout(Duration::from_millis(100), async {
        sleep(Duration::from_millis(50)).await;
        "Operation completed"
    })
    .await;
    match result {
        Ok(msg) => println!("Success: {}", msg),
        Err(_) => println!("Operation timed out"),
    }

    // Example 4: Timeout that expires
    println!("\nExample 4: Timeout that expires");
    let result = timeout(Duration::from_millis(50), async {
        sleep(Duration::from_millis(100)).await;
        "This should not be printed"
    })
    .await;
    match result {
        Ok(msg) => println!("Success: {}", msg),
        Err(_) => println!("Operation timed out (expected)"),
    }

    println!("\nAll timer examples completed!");
    Ok(())
}