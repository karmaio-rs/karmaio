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
