//! Runtime configuration and builder.
//!
//! [`RuntimeBuilder`] follows the builder pattern:
//! construct it with [`RuntimeBuilder::new`], tweak knobs via the fluent [`must_use`] setters,
//! then call [`RuntimeBuilder::build`] to obtain a [`Runtime`].
//!
//! Every knob has a sensible default value if don't wish to provide one.

use std::time::Duration;

use crate::runtime::local::Runtime;

/// Configuration for a [`Runtime`].
///
/// Created by [`RuntimeBuilder`] and consumed when the runtime is built.
/// The knobs here describe runtime-global resources created at build time
/// (the blocking pool and the platform driver).
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum number of worker threads in the blocking pool.
    pub(crate) blocking_threads: usize,
    /// Idle keep-alive before a blocking worker thread exits.
    pub(crate) blocking_keep_alive: Duration,
    /// Capacity of the platform driver's op slab + I/O entries/events.
    pub(crate) driver_capacity: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            blocking_threads: 256,
            blocking_keep_alive: Duration::from_secs(60),
            driver_capacity: 1024,
        }
    }
}

/// Builder for a [`Runtime`].
///
/// Create with [`RuntimeBuilder::new`], configure with the fluent setters, then
/// call [`RuntimeBuilder::build`].
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use karmaio::builder::RuntimeBuilder;
///
/// let mut rt = RuntimeBuilder::new()
///     .blocking_threads(128)
///     .blocking_keep_alive(Duration::from_secs(30))
///     .driver_capacity(2048)
///     .build()
///     .unwrap();
///
/// rt.block_on(async { /* ... */ });
/// ```
pub struct RuntimeBuilder {
    config: RuntimeConfig,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create a builder with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: RuntimeConfig::default(),
        }
    }

    /// Set the maximum number of worker threads in the blocking thread pool.
    ///
    /// Defaults to [`256`] (256)
    #[must_use]
    pub fn blocking_threads(mut self, threads: usize) -> Self {
        self.config.blocking_threads = threads.max(1);
        self
    }

    /// Set the idle keep-alive duration before a blocking worker thread exits.
    ///
    /// Defaults to [`Duration::from_secs(60)`] (60s).
    #[must_use]
    pub fn blocking_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.config.blocking_keep_alive = keep_alive;
        self
    }

    /// Set the capacity of the platform driver.
    ///
    /// This controls the size of the op-tracking slab and the I/O entries/events
    /// buffer used by the underlying platform backend (io_uring / IOCP /
    /// kqueue). Defaults to [`1024`] (1024).
    #[must_use]
    pub fn driver_capacity(mut self, capacity: usize) -> Self {
        self.config.driver_capacity = capacity.max(1);
        self
    }

    /// Build the configured [`Runtime`].
    pub fn build(self) -> std::io::Result<Runtime> {
        Runtime::from_config(self.config)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::local::Runtime;

    #[test]
    fn builder_produces_runnable_runtime() {
        let mut rt = RuntimeBuilder::new()
            .blocking_threads(64)
            .blocking_keep_alive(Duration::from_secs(10))
            .driver_capacity(2048)
            .build()
            .expect("runtime should build");

        rt.block_on(async { 7usize });
    }

    #[test]
    fn default_builder_matches_runtime_new() {
        // `Runtime::new` should be equivalent to the default builder.
        let mut rt = Runtime::new().expect("runtime should build");
        rt.block_on(async { 1usize });
    }

    #[test]
    fn runtime_builder_entrypoint_works() {
        let mut rt = Runtime::builder().build().expect("runtime should build");
        rt.block_on(async { 2usize });
    }

    #[test]
    fn defaults_are_sane() {
        let config = RuntimeConfig::default();
        assert_eq!(config.blocking_threads, 256);
        assert_eq!(config.blocking_keep_alive, Duration::from_secs(60));
        assert_eq!(config.driver_capacity, 1024);
    }
}
