use std::time::{Duration, Instant};

use super::driver::create_timer;

/// Waits until `duration` has elapsed.
///
/// Equivalent to [`sleep_until(Instant::now() + duration)`](sleep_until). An
/// asynchronous analog to [`std::thread::sleep`].
///
/// To run something regularly on a schedule, see [`super::interval`].
///
/// Timer futures are tied to the current runtime thread and must be used from
/// within a [`Runtime`](crate::runtime::local::Runtime).
pub async fn sleep(duration: Duration) {
    sleep_until(Instant::now() + duration).await;
}

/// Waits until `deadline` is reached.
///
/// To run something regularly on a schedule, see [`super::interval`].
///
/// Timer futures are tied to the current runtime thread and must be used from
/// within a [`Runtime`](crate::runtime::local::Runtime).
pub async fn sleep_until(deadline: Instant) {
    create_timer(deadline).await;
}