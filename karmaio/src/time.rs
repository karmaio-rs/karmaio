//! Utilities for tracking time.
//!
//! This module provides async utilities for executing code after a set period of
//! time. These types must be used from within the context of the
//! [`Runtime`](crate::runtime::local::Runtime).
//!
//! # Examples
//!
//! ```
//! use std::time::Duration;
//!
//! use karmaio::runtime::local::Runtime;
//! use karmaio::time::sleep;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut runtime = Runtime::new()?;
//! runtime.block_on(async {
//!     sleep(Duration::from_millis(100)).await;
//!     println!("100 ms have elapsed");
//! });
//! # Ok(())
//! # }
//! ```

mod driver;
mod interval;
mod sleep;
mod timeout;

pub use self::interval::{Interval, interval, interval_at};
pub use self::sleep::{sleep, sleep_until};
pub use self::timeout::{Elapsed, timeout, timeout_at};

pub use std::time::{Duration, Instant};

pub(crate) use driver::Timer;
