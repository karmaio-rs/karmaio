# Changelog

All notable changes to this project will be documented in this file.

The project follows [Semantic Versioning](https://semver.org/).
During the `0.1.0` alpha series, APIs and behavior may change between prereleases.

## [Unreleased]

## [0.1.0-alpha.1] - 2026-08-15

### Added

- A multithreaded, share-nothing asynchronous runtime with io_uring, IOCP, and kqueue backends.
- Runtime task spawning, blocking work, timers, and asynchronous I/O traits.
- Feature-gated filesystem, networking, process, signal, and attribute-macro APIs.
- Linux io_uring managed-buffer and multishot accept/receive APIs.

[Unreleased]: https://github.com/karmaio-rs/karmaio/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.1]: https://github.com/karmaio-rs/karmaio/releases/tag/v0.1.0-alpha.1
