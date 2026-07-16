//! Demonstrates custom runtime configuration with karmaio.
//!
//! This example shows how to:
//! - Create a runtime with custom configuration
//! - Configure blocking thread pool
//! - Configure driver capacity
//! - Use the builder pattern
//!
//! Run the example:
//!
//!     cargo run --example runtime_builder

use std::time::Duration;

use karmaio::builder::RuntimeBuilder;
use karmaio::runtime::Runtime;

fn main() {
    // Example 1: Default runtime
    println!("Example 1: Default runtime");
    let mut rt = Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        println!("Running on default runtime");
    });

    // Example 2: Custom runtime with builder
    println!("\nExample 2: Custom runtime with builder");
    let mut rt = RuntimeBuilder::new()
        .blocking_threads(64)
        .blocking_keep_alive(Duration::from_secs(30))
        .driver_capacity(2048)
        .build()
        .expect("Failed to create custom runtime");
    
    rt.block_on(async {
        println!("Running on custom runtime");
    });

    // Example 3: Runtime with minimal configuration
    println!("\nExample 3: Minimal runtime");
    let mut rt = RuntimeBuilder::new()
        .blocking_threads(4)
        .driver_capacity(256)
        .build()
        .expect("Failed to create minimal runtime");
    
    rt.block_on(async {
        println!("Running on minimal runtime");
    });

    // Example 4: Using spawn and block_on
    println!("\nExample 4: Spawn and block_on");
    let mut rt = Runtime::new().expect("Failed to create runtime");
    let handle = rt.spawn(async {
        // Spawn a task
        println!("Spawned task running");
        42
    });
    
    rt.block_on(async {
        let result = handle.await.expect("Task failed");
        println!("Spawned task returned: {}", result);
    });

    // Example 5: Using spawn_blocking
    println!("\nExample 5: Spawn blocking");
    let mut rt = Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        let handle = karmaio::runtime::spawn_blocking(|| {
            // This runs on a blocking thread
            std::thread::sleep(Duration::from_millis(100));
            println!("Blocking task completed");
            "blocking result"
        });
        
        let result = handle.await.expect("Blocking task failed");
        println!("Blocking task returned: {}", result);
    });

    println!("\nAll runtime builder examples completed!");
}