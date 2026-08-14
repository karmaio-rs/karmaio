# karmaio

A modern fast multi-threaded share-nothing asynchronous runtime for Rust.

### Currently in progress.

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
| `full`    | All of the above                                                        |
| `default` | Empty (nothing enabled)                                                 |

### Recommended dependency lines

```toml
# Full public API (apps / demos).
karmaio = { version = "0.1", features = ["full"] }

# Core runtime only.
karmaio = "0.1"

# Networking + attribute macros.
karmaio = { version = "0.1", features = ["macros", "net"] }

# Filesystem + signals.
karmaio = { version = "0.1", features = ["fs", "signal"] }
```

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

See module docs on `karmaio::buf` and the `echo_tcp_recv_multi` example.
