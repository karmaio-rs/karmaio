//! Async timers: `sleep`, `interval`, and `timeout`.
//!
//! Note on `timeout`: when the deadline fires, the inner future is **dropped**.
//! For ordinary I/O that means the op is cancelled and the buffer is forfeited.
//! To recover it, keep a pinned [`karmaio::runtime::FutureExt::with_cancellation`]
//! future, request [`karmaio::runtime::CancellationSource::cancel`] when the timer
//! fires, then await that same future. See the runtime docs on I/O cancellation.
//!
//! ```text
//! cargo run --example timer
//! ```

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
        println!("Tick {i} at {:?}", std::time::Instant::now());
    }

    // Example 3: Timeout that succeeds
    println!("\nExample 3: Timeout that succeeds");
    let result = timeout(Duration::from_millis(100), async {
        sleep(Duration::from_millis(50)).await;
        "Operation completed"
    })
    .await;
    match result {
        Ok(msg) => println!("Success: {msg}"),
        Err(_) => println!("Operation timed out"),
    }

    // Example 4: Timeout that expires (inner future is dropped / detached)
    println!("\nExample 4: Timeout that expires");
    let result = timeout(Duration::from_millis(50), async {
        sleep(Duration::from_millis(100)).await;
        "This should not be printed"
    })
    .await;
    match result {
        Ok(msg) => println!("Success: {msg}"),
        Err(_) => println!("Operation timed out (expected; inner future was dropped)"),
    }

    println!("\nAll timer examples completed!");
    Ok(())
}
