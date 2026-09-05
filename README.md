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
| `tls`     | Rustls TLS with `ring` and TLS 1.2                    |
| `tls-rustls` | Rustls TLS without selecting a cryptographic provider |
| `tls-ring` | Rustls TLS with the `ring` provider                  |
| `tls-aws-lc-rs` | Rustls TLS with the AWS-LC provider             |
| `tls12`   | TLS 1.2 for either Rustls provider                    |
| `full`    | All subsystems, selecting `tls` for TLS                |
| `default` | Empty                                                |

```toml
# Full public API.
karmaio = { version = "0.1.0-alpha.4", features = ["full"] }

# Core runtime only (default).
karmaio = "0.1.0-alpha.4"

# Networking + attribute macros.
karmaio = { version = "0.1.0-alpha.4", features = ["macros", "net"] }

# Networking + batteries-included Rustls TLS.
karmaio = { version = "0.1.0-alpha.4", features = ["macros", "net", "tls"] }
```

TLS is driven directly over Karmaio's owned-buffer I/O traits.
The convenience feature does not itself imply `net` or `bytes`; advanced users can choose `tls-ring` or `tls-aws-lc-rs` and add `tls12` independently.
Callers provide fully configured Rustls client or server configurations, including roots, identity, SNI, and ALPN policy.
Karmaio does not mutate Rustls's global provider.

TLS streams are intentionally not splittable. Vectored reads fill caller components in order; scalar and vectored writes are bounded and write-through.
Shutdown sends `close_notify`, and a transport EOF without the peer's `close_notify` is surfaced as `UnexpectedEof`.
Cancellation returns the original caller-owned buffer and conservatively poisons the stream when wire progress may be ambiguous.
Dropping an established I/O future while it awaits the transport also makes later operations fail closed. See the `tls_client` and `tls_server` examples.
`into_parts` performs no shutdown and refuses to extract state after either direction fails.

See the [crate documentation](https://docs.rs/karmaio), the [examples](https://github.com/karmaio-rs/karmaio/tree/main/examples), and the [changelog](CHANGELOG.md) for more details.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
