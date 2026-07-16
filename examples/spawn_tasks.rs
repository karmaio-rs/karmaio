//! Spawning tasks, joining, aborting, and clean runtime shutdown.
//!
//! Covers the join-handle contract documented on [`Runtime`] and
//! [`JoinHandle`]:
//!
//! - Dropping a [`JoinHandle`] **detaches** the task (it keeps running).
//! - [`JoinHandle::abort`] cooperatively cancels a task.
//! - Await (or drop) handles you care about **before** dropping the
//!   [`Runtime`], or a later `.await` will hang.
//!
//! ```text
//! cargo run --example spawn_tasks
//! ```

use std::future::pending;
use std::time::Duration;

use karmaio::runtime::{JoinHandle, Runtime};
use karmaio::time::sleep;

fn main() {
    // ------------------------------------------------------------------
    // 1. Spawn + await (join)
    // ------------------------------------------------------------------
    println!("Example 1: spawn and await");
    {
        let mut rt = Runtime::new().expect("runtime");
        let handle: JoinHandle<u32> = rt.spawn(async {
            sleep(Duration::from_millis(20)).await;
            7
        });

        let value = rt.block_on(async { handle.await.expect("task should succeed") });
        println!("  joined value = {value}");
        // Runtime drops here after the join — clean shutdown.
    }

    // ------------------------------------------------------------------
    // 2. Abort before the task finishes
    // ------------------------------------------------------------------
    println!("\nExample 2: abort");
    {
        let mut rt = Runtime::new().expect("runtime");
        let handle = rt.spawn(pending::<()>());
        handle.abort();

        let err = rt
            .block_on(async { handle.await })
            .expect_err("aborted task should error");
        assert!(err.is_cancelled());
        println!("  abort reported cancelled: {}", err);
    }

    // ------------------------------------------------------------------
    // 3. Detach: drop the JoinHandle, task keeps running
    // ------------------------------------------------------------------
    println!("\nExample 3: detach (drop JoinHandle)");
    {
        let mut rt = Runtime::new().expect("runtime");

        // Dropping the handle detaches; the task is not cancelled.
        let _ = rt.spawn(async {
            sleep(Duration::from_millis(30)).await;
            println!("  detached task finished while runtime still drove work");
        });

        rt.block_on(async {
            // Give the detached task time to complete under this runtime.
            sleep(Duration::from_millis(50)).await;
        });
    }

    // ------------------------------------------------------------------
    // 4. Clean multi-task shutdown
    // ------------------------------------------------------------------
    println!("\nExample 4: await all handles before dropping Runtime");
    {
        let mut rt = Runtime::new().expect("runtime");

        let a = rt.spawn(async {
            sleep(Duration::from_millis(10)).await;
            "a"
        });
        let b = rt.spawn(async {
            sleep(Duration::from_millis(15)).await;
            "b"
        });

        // Collect results while the runtime is still alive.
        let (ra, rb) = rt.block_on(async { (a.await.expect("task a"), b.await.expect("task b")) });
        println!("  results: {ra}, {rb}");
        // Dropping `rt` now is safe: nothing is left awaiting this runtime.
    }

    // ------------------------------------------------------------------
    // 5. spawn_blocking
    // ------------------------------------------------------------------
    println!("\nExample 5: spawn_blocking");
    {
        let mut rt = Runtime::new().expect("runtime");
        rt.block_on(async {
            let handle = karmaio::runtime::spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(20));
                99
            });
            let n = handle.await.expect("blocking job");
            println!("  blocking pool returned {n}");
        });
    }

    println!("\nAll spawn / join examples completed.");
}
