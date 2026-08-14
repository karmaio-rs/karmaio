#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
pub(crate) mod macros;

pub mod buf;
pub mod builder;
pub mod io;
pub mod runtime;
pub mod time;

#[cfg(feature = "fs")]
#[cfg_attr(docsrs, doc(cfg(feature = "fs")))]
pub mod fs;

#[cfg(feature = "net")]
#[cfg_attr(docsrs, doc(cfg(feature = "net")))]
pub mod net;

#[cfg(feature = "process")]
#[cfg_attr(docsrs, doc(cfg(feature = "process")))]
pub mod process;

#[cfg(feature = "signal")]
#[cfg_attr(docsrs, doc(cfg(feature = "signal")))]
pub mod signal;

pub(crate) mod driver;
pub(crate) mod slab;
pub(crate) mod task;

pub use builder::{RuntimeBuilder, RuntimeConfig};
pub use runtime::{JoinError, JoinHandle, Runtime};

/// Attribute macros that turn an `async fn` into a runtime-driven entrypoint.
///
/// Requires the `macros` feature.
///
/// `#[karmaio::main]` builds a [`RuntimeBuilder`] and drives the future with
/// [`Runtime::block_on`](crate::runtime::local::Runtime::block_on).
/// `#[karmaio::test]` does the same for `#[test]` functions.
/// Builder methods can be configured via attribute arguments, e.g.
/// `#[karmaio::main(blocking_threads = 64, driver_capacity = 2048)]`.
///
/// ```rust
/// # #[cfg(not(feature = "macros"))]
/// # fn main() {}
/// # #[cfg(feature = "macros")]
/// #[karmaio::main]
/// async fn main() {
///     let answer = async { 2 * 21 }.await;
///     assert_eq!(answer, 42);
/// }
/// ```
#[cfg(feature = "macros")]
#[cfg_attr(docsrs, doc(cfg(feature = "macros")))]
pub use karmaio_macros::{main, test};
