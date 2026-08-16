# karmaio

A modern, fast, share-nothing asynchronous runtime for Rust, using io-uring on Linux, IOCP on Windows, and kqueue on macOS and BSDs.

> **Alpha status:** This is an early release intended for evaluation and experimentation.
> APIs and runtime behavior may change between alpha releases.

## Feature flags

Heavy subsystems are feature-gated so consumers can trim compile time and binary size. **Defaults are empty**.

| Feature   | What it enables                                      |
|-----------|------------------------------------------------------|
| `fs`      | Filesystem APIs and path/file driver ops             |
| `macros`  | `#[karmaio::main]` / `#[karmaio::test]`              |
| `net`     | TCP / UDP / Unix sockets (`socket2`)                 |
| `process` | Child processes and async stdio                      |
| `signal`  | Ctrl-C and Unix signal handling                      |
| `bytes`   | `IoBuf` / `IoBufMut` for `bytes::{Bytes, BytesMut}`  |
| `memmap2` | `IoBuf` / `IoBufMut` for `memmap2::{Mmap, MmapMut}`  |
| `full`    | All of the above                                     |
| `default` | Empty                                                |

```toml
# Full public API.
karmaio = { version = "0.1.0-alpha.1", features = ["full"] }

# Core runtime only (default).
karmaio = "0.1.0-alpha.1"

# Networking + attribute macros.
karmaio = { version = "0.1.0-alpha.1", features = ["macros", "net"] }
```

See the [crate documentation](https://docs.rs/karmaio), the [examples](https://github.com/karmaio-rs/karmaio/tree/main/examples), and the [changelog](CHANGELOG.md) for more details.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
