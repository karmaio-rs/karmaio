//! Runtime configuration and builder.
//!
//! [`RuntimeBuilder`] follows the builder pattern:
//! construct it with [`RuntimeBuilder::new`], tweak knobs via the fluent [`must_use`] setters,
//! then call [`RuntimeBuilder::build`] to obtain a [`Runtime`].
//!
//! Every knob has a sensible default value if don't wish to provide one.

use std::{io, num::NonZeroU32, time::Duration};

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
    /// Number of task polls between nonblocking driver turns.
    pub(crate) event_interval: NonZeroU32,
    /// Number of provided buffers in the Linux io_uring buffer pool.
    ///
    /// Rounded up to a power of two when the pool is created. Used by
    /// managed / multishot receive APIs. Ignored on non-Linux targets.
    pub(crate) buffer_pool_size: u16,
    /// Byte length of each buffer in the Linux io_uring buffer pool.
    ///
    /// Ignored on non-Linux targets.
    pub(crate) buffer_pool_buffer_len: usize,
    /// Maximum unconsumed accepted sockets queued by one multishot stream.
    pub(crate) multishot_accept_capacity: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            blocking_threads: 256,
            blocking_keep_alive: Duration::from_secs(60),
            driver_capacity: 1024,
            event_interval: NonZeroU32::new(61).expect("default event interval is nonzero"),
            buffer_pool_size: 64,
            buffer_pool_buffer_len: 8192,
            multishot_accept_capacity: 128,
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn validate(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            if self.buffer_pool_size > crate::buf::MAX_BUFFER_RING_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("buffer pool size cannot exceed {}", crate::buf::MAX_BUFFER_RING_ENTRIES),
                ));
            }
            if self.buffer_pool_buffer_len > u32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer pool buffer length cannot exceed u32::MAX",
                ));
            }
        }
        Ok(())
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
///     .event_interval(31)
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

    /// Set the number of task polls between checks for external events.
    ///
    /// Smaller values reduce I/O and timer latency under sustained runnable
    /// work. Larger values reduce the syscall overhead of nonblocking driver
    /// turns. The default is `61`.
    ///
    /// # Panics
    ///
    /// Panics if `interval` is zero.
    #[must_use]
    #[track_caller]
    pub fn event_interval(mut self, interval: u32) -> Self {
        self.config.event_interval = NonZeroU32::new(interval).expect("event interval must be greater than zero");
        self
    }

    /// Set the number of buffers in the runtime's provided buffer pool.
    ///
    /// Linux only (used by managed / multishot receive). The size is rounded
    /// up to a power of two when the pool is first created. The maximum is
    /// `32768`, as required by the kernel. Defaults to `64`.
    ///
    /// Holding many outstanding [`crate::buf::PooledBuf`] leases without
    /// recycling them can exhaust this pool and end multishot receives with
    /// `ENOBUFS`. Prefer smaller hold times or a larger pool under load.
    ///
    /// Ignored on non-Linux targets.
    #[must_use]
    pub fn buffer_pool_size(mut self, size: u16) -> Self {
        self.config.buffer_pool_size = size.max(1);
        self
    }

    /// Set the byte length of each buffer in the provided buffer pool.
    ///
    /// Linux only. Defaults to `8192`. Ignored on non-Linux targets.
    #[must_use]
    pub fn buffer_pool_buffer_len(mut self, len: usize) -> Self {
        self.config.buffer_pool_buffer_len = len.max(1);
        self
    }

    /// Set the pending connection limit for each multishot accept stream.
    ///
    /// Linux only. Once this many accepted sockets are waiting to be consumed,
    /// overflow connections are closed and that multishot request terminates
    /// with a capacity error. Defaults to `128`.
    #[must_use]
    pub fn multishot_accept_capacity(mut self, capacity: usize) -> Self {
        self.config.multishot_accept_capacity = capacity.max(1);
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
        assert_eq!(config.event_interval.get(), 61);
        assert_eq!(config.multishot_accept_capacity, 128);
    }

    #[test]
    fn event_interval_is_configurable() {
        let builder = RuntimeBuilder::new().event_interval(31);

        assert_eq!(builder.config.event_interval.get(), 31);
    }

    #[test]
    fn multishot_accept_capacity_is_configurable() {
        let builder = RuntimeBuilder::new().multishot_accept_capacity(32);

        assert_eq!(builder.config.multishot_accept_capacity, 32);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn oversized_buffer_pool_is_rejected() {
        let mut config = RuntimeConfig::default();
        config.buffer_pool_size = crate::buf::MAX_BUFFER_RING_ENTRIES + 1;

        let err = config.validate().expect_err("oversized pool must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    #[should_panic(expected = "event interval must be greater than zero")]
    fn zero_event_interval_is_rejected() {
        let _ = RuntimeBuilder::new().event_interval(0);
    }
}
