//! Custom runtime configuration and the builder pattern.
//!
//! Also shows a clean shutdown sequence: finish `block_on` / await
//! [`JoinHandle`](karmaio::JoinHandle)s **before** dropping the [`Runtime`].
//!
//! ```text
//! cargo run --example runtime_builder
//! ```

use std::time::Duration;

use karmaio::builder::RuntimeBuilder;
use karmaio::runtime::Runtime;
use karmaio::time::sleep;

fn main() {
    // Example 1: Default runtime
    println!("Example 1: Default runtime");
    let mut rt = Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        println!("  running on default runtime");
    });
    // `rt` drops here after block_on returns — nothing left to join.

    // Example 2: Custom runtime with builder
    println!("\nExample 2: Custom runtime with builder");
    let mut rt = RuntimeBuilder::new()
        .blocking_threads(64)
        .blocking_keep_alive(Duration::from_secs(30))
        .driver_capacity(2048)
        .build()
        .expect("Failed to create custom runtime");

    rt.block_on(async {
        println!("  running on custom runtime");
    });

    // Example 3: Minimal configuration
    println!("\nExample 3: Minimal runtime");
    let mut rt = RuntimeBuilder::new()
        .blocking_threads(4)
        .driver_capacity(256)
        .build()
        .expect("Failed to create minimal runtime");

    rt.block_on(async {
        println!("  running on minimal runtime");
    });

    // Example 4: Spawn, await, then drop (clean shutdown)
    println!("\nExample 4: Spawn, await, then drop Runtime");
    {
        let mut rt = Runtime::new().expect("Failed to create runtime");
        let handle = rt.spawn(async {
            sleep(Duration::from_millis(10)).await;
            println!("  spawned task running");
            42
        });

        let result = rt.block_on(async { handle.await.expect("Task failed") });
        println!("  spawned task returned: {result}");
        // All joins done; dropping `rt` is safe.
    }

    // Example 5: spawn_blocking
    println!("\nExample 5: spawn_blocking");
    let mut rt = Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        let handle = karmaio::runtime::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(100));
            println!("  blocking task completed");
            "blocking result"
        });

        let result = handle.await.expect("Blocking task failed");
        println!("  blocking task returned: {result}");
    });

    println!("\nAll runtime builder examples completed!");
}
