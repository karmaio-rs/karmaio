# Changelog

All notable changes to this project will be documented in this file.

The project follows [Semantic Versioning](https://semver.org/).
During the `0.1.0` alpha series, APIs and behavior may change between prereleases.

## [Unreleased]

### Added

- Added `runtime::{CancellationSource, CancellationToken, FutureExt}` for fail-slow eager cancellation of ordinary I/O futures and helpers. Tokens are copyable and observation-only, one source can cover many operations, nested tokens compose, and lazily submitted multishot streams can be wrapped through `runtime::StreamExt`.
- Added `FramedReadParts`, `FramedWriteParts`, and `FramedParts` for lossless decomposition of framed adapters. `into_parts()` extracts the transport, codec, framer, and buffers; `from_parts()` validates the unread range and reconstructs the adapter.

### Changed

- Dropping a submitted one-shot I/O future now requests best-effort platform cancellation before detaching. The runtime retains kernel-owned payloads until terminal completion.
- Platform cancellation errors are normalized to `OperationCanceled` at the operation boundary and remain classifiable with `is_operation_canceled`.
- `Framer::enclose` is now fallible (`io::Result<()>`), propagating buffer growth and payload-size errors through `Sink::send` instead of panicking.

### Removed

- Breaking alpha API change: removed `Canceller`, `CancelHandle`, `AsyncReadCancellable`, `AsyncWriteCancellable`, and the parallel `*_cancellable` socket and vectored-I/O methods. Wrap the ordinary operation with `with_cancellation(token)` instead.

## [0.1.0-alpha.3] - 2026-08-21

### Added

- Owned stream splitting for TCP and Unix streams via `split` APIs, allowing independent read/write halves without borrowing.
- Cancellable I/O for sockets: cancel in-flight TCP, UDP, and Unix socket operations across all driver backends (io_uring, IOCP, kqueue).
- Added a Vectored write-all API including cancellation support.

### Changed

- Socket receive/send driver ops now support cancellation on all platforms.

### Added

- Optional `bytes` feature with `IoBuf` impls for `bytes::Bytes` / `bytes::BytesMut` and `IoBufMut` / `SetLen` for `bytes::BytesMut`.
- Optional `memmap2` feature with `IoBuf` impls for `memmap2::Mmap` / `memmap2::MmapMut` and `IoBufMut` / `SetLen` for `memmap2::MmapMut`.

## [0.1.0-alpha.1] - 2026-08-15

### Added

- A multithreaded, share-nothing asynchronous runtime with io_uring, IOCP, and kqueue backends.
- Runtime task spawning, blocking work, timers, and asynchronous I/O traits.
- Feature-gated filesystem, networking, process, signal, and attribute-macro APIs.
- Linux io_uring managed-buffer and multishot accept/receive APIs.

[Unreleased]: https://github.com/karmaio-rs/karmaio/compare/v0.1.0-alpha.3...HEAD
[0.1.0-alpha.1]: https://github.com/karmaio-rs/karmaio/releases/tag/v0.1.0-alpha.1
[0.1.0-alpha.2]: https://github.com/karmaio-rs/karmaio/releases/tag/v0.1.0-alpha.2
[0.1.0-alpha.3]: https://github.com/karmaio-rs/karmaio/releases/tag/v0.1.0-alpha.3
