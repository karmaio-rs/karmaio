# karmaio

A modern, fast, share-nothing asynchronous runtime for Rust, using io_uring on Linux, IOCP on Windows, and kqueue on macOS and BSDs.

> **Alpha status:** This is an early release intended for evaluation and experimentation.
> APIs and runtime behavior may change between alpha releases.

## Feature flags

Heavy subsystems are opt-in via Cargo features.
The core runtime (`Runtime`, tasks, timers, `io` traits, buffers, and the platform driver) is always available.
**No features are enabled by default.**

| Feature   | Enables                                                                 |
|-----------|-------------------------------------------------------------------------|
| `fs`      | `karmaio::fs` and filesystem driver ops (open, path ops, positioned I/O) |
| `macros`  | `#[karmaio::main]` / `#[karmaio::test]` (optional `karmaio-macros` dep) |
| `net`     | `karmaio::net` (TCP/UDP/Unix), socket ops, and the `socket2` dependency |
| `process` | `karmaio::process`, child stdio pipe I/O, and process-wait ops          |
| `signal`  | `karmaio::signal` (Ctrl-C and Unix signals)                             |
| `bytes`   | `IoBuf` / `IoBufMut` impls for `bytes::{Bytes, BytesMut}`               |
| `memmap2` | `IoBuf` / `IoBufMut` impls for `memmap2::{Mmap, MmapMut}`               |
| `tls`     | Rustls TLS with `ring` and TLS 1.2                                     |
| `tls-rustls` | Rustls TLS without selecting a cryptographic provider               |
| `tls-ring` | Rustls TLS with the `ring` provider                                   |
| `tls-aws-lc-rs` | Rustls TLS with the AWS-LC provider                              |
| `tls12`   | TLS 1.2 for either Rustls provider                                     |
| `full`    | All subsystems, selecting `tls` rather than both TLS providers          |
| `default` | Empty (nothing enabled)                                                 |

### Recommended dependency lines

```toml
# Full public API (apps / demos).
karmaio = { version = "0.1.0-alpha.4", features = ["full"] }

# Core runtime only.
karmaio = "0.1.0-alpha.4"

# Networking + attribute macros.
karmaio = { version = "0.1.0-alpha.4", features = ["macros", "net"] }

# Filesystem + signals.
karmaio = { version = "0.1.0-alpha.4", features = ["fs", "signal"] }

# TCP and batteries-included Rustls TLS.
karmaio = { version = "0.1.0-alpha.4", features = ["macros", "net", "tls"] }
```

## TLS

`karmaio::tls` drives Rustls directly over Karmaio's owned-buffer I/O traits. The `tls` convenience feature selects `ring`, TLS 1.3, and TLS 1.2; applications can instead select `tls-ring` or `tls-aws-lc-rs`, with optional `tls12`.
TLS does not imply `net` or `bytes`.

Applications own verification policy. Supply an `Arc<ClientConfig>` or `Arc<ServerConfig>` with an explicit cryptographic provider, trust roots,
identity, SNI, and ALPN policy. Karmaio does not install a global provider, discover roots, or offer an insecure verifier.

TLS streams preserve the completion model and add no `Send` requirement. They are deliberately not splittable because reads may need to write protocol messages.
Vectored I/O operates directly on caller components, and successful scalar and vectored writes are write-through. Adapter-owned ciphertext buffers have fixed capacity, while Rustls retains its default internal buffering policy.
Graceful shutdown sends `close_notify`.
A raw transport EOF without peer `close_notify` is reported as `UnexpectedEof` after buffered plaintext is delivered. `into_parts` is abrupt, performs no TLS or transport shutdown, and refuses to extract state after either direction fails.

Cancellation scopes propagate to the underlying operation and return caller buffers through `BufResult`.
For established I/O, request cancellation and await the same future instead of dropping a timed-out future when buffer recovery matters.
If an established I/O future is dropped in flight, later stream operations fail closed because transport progress is ambiguous.
See the workspace `tls_client` and `tls_server` examples.

## Linux multishot I/O (6.12+)

On Linux, karmaio exposes explicit multishot and managed-buffer APIs (io_uring).
There is **no kernel version probe**; callers must meet the **6.12+** floor.

| API | Purpose |
|-----|---------|
| `TcpListener::incoming` / `UnixListener::incoming` | Multishot accept stream |
| `TcpStream::recv_managed` / `recv_multi` (and Unix) | Pool-buffer oneshot / multishot stream receive |
| `UdpSocket` managed / multishot receive methods | `RecvDatagram` with payload, peer, flags, and original length |

Classic `read` / `recv` with user-owned buffers and `BufResult` remain unchanged.

### Buffer pool and leases

Managed receives use a **per-runtime** provided buffer pool (defaults: **64 buffers × 8192 bytes**, configured via `RuntimeBuilder::buffer_pool_size` / `buffer_pool_buffer_len`).

- `karmaio::buf::PooledBuf` is a **lease**, not an owned allocation: drop it or call `release()` to return the slot to the pool.
- Holding many leases without recycling can exhaust the pool; multishot streams then end with **`ENOBUFS`** (surfaced as an error item, then stream end). Multishot requests are **not auto-rearmed** — call `recv_multi` again after recycling if needed.
- The pool is shared by all sockets on one runtime; retaining leases for one socket can starve unrelated sockets.

See the module docs on `karmaio::buf` and the [`echo_tcp_recv_multi` example](https://github.com/karmaio-rs/karmaio/blob/main/examples/echo_tcp_recv_multi.rs).

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
