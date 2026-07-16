#[macro_use]
pub mod macros;

pub mod buf;
pub mod builder;
pub mod fs;
pub mod io;
pub mod net;
pub mod process;
pub mod runtime;
pub mod signal;
pub mod time;

pub(crate) mod driver;
pub(crate) mod task;

pub use builder::{RuntimeBuilder, RuntimeConfig};
pub use runtime::{JoinError, JoinHandle, Runtime};

/// Attribute macros that turn an `async fn` into a runtime-driven entrypoint.
///
/// `#[karmaio::main]` builds a [`RuntimeBuilder`] and drives the future with
/// [`Runtime::block_on`](crate::runtime::local::Runtime::block_on).
/// `#[karmaio::test]` does the same for `#[test]` functions.
/// Builder methods can be configured via attribute arguments, e.g.
/// `#[karmaio::main(blocking_threads = 64, driver_capacity = 2048)]`.
///
/// ```rust
/// #[karmaio::main]
/// async fn main() {
///     let answer = async { 2 * 21 }.await;
///     assert_eq!(answer, 42);
/// }
/// ```
pub use karmaio_macros::{main, test};
