# karmaio

A modern fast multi-threaded share-nothing asynchronous runtime for Rust,
using io-uring on Linux, IOCP on Windows, and kqueue on macOS and BSDs.

### Currently in progress.

## Feature flags

Heavy subsystems are feature-gated so consumers can trim compile time and binary size.
**Defaults are empty** (same idea as Tokio).

| Feature   | What it enables                                      |
|-----------|------------------------------------------------------|
| `fs`      | Filesystem APIs and path/file driver ops             |
| `macros`  | `#[karmaio::main]` / `#[karmaio::test]`              |
| `net`     | TCP / UDP / Unix sockets (`socket2`)                 |
| `process` | Child processes and async stdio                      |
| `signal`  | Ctrl-C and Unix signal handling                      |
| `full`    | All of the above                                     |
| `default` | Empty                                                |

```toml
# Full public API.
karmaio = { version = "0.1", features = ["full"] }

# Core runtime only (default).
karmaio = "0.1"

# Networking + attribute macros.
karmaio = { version = "0.1", features = ["macros", "net"] }
```
