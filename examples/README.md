# karmaio Examples

Examples for the main features of the karmaio runtime.

## Running Examples

From the workspace root:

```bash
cargo run -p karmaio-examples --example <example_name>
```

Or from this directory:

```bash
cd examples
cargo run --example <example_name>
```

## Examples

### Networking

| Example | Description |
|---------|-------------|
| **echo_tcp** | TCP echo server; handles each connection with `spawn_local` (detached tasks). |
| **echo_tcp_multi** | Same as `echo_tcp`, but uses `TcpListener::incoming` (Linux multishot accept under the hood). Requires Linux 6.12+. |
| **echo_tcp_recv_multi** | TCP echo with `incoming()` + `TcpStream::recv_multi` (pool buffers / multishot recv). Requires Linux 6.12+. Recycle `PooledBuf` leases promptly. |
| **accept_multi** | Minimal `incoming()` demo on `127.0.0.1:8081`. Requires Linux 6.12+. |
| **hello_world** | TCP client; pairs with `echo_tcp` / `echo_tcp_multi` on `127.0.0.1:8080`. |
| **udp_echo** | UDP echo server on `127.0.0.1:8080`. |
| **udp_client** | UDP client; pairs with `udp_echo`. |
| **tls_server** | Rustls TLS echo server with explicit provider, DER identity, and ALPN. |
| **tls_client** | Certificate-verifying Rustls TLS client with SNI and ALPN. |
| **tls_split_client** | TLS client using independently progressing owned halves, with reads in a local task. |

### Tasks & runtime

| Example | Description |
|---------|-------------|
| **spawn_tasks** | Spawn, join, abort, detach, and clean `Runtime` shutdown. |
| **runtime_builder** | Builder config, spawn, `spawn_blocking`, and drop order. |
| **timer** | `sleep`, `interval`, `timeout` (notes I/O detach on timeout). |

### File I/O

| Example | Description |
|---------|-------------|
| **file_operations** | Create, read, write, rename, remove files. |

### Process & signals

| Example | Description |
|---------|-------------|
| **process** | Spawn processes, pipes, capture output. |
| **signal** | Ctrl-C and Unix signals (`SIGTERM` / `SIGHUP`). |

## Suggested pairings

```bash
# TCP (oneshot accept)
cargo run --example echo_tcp          # terminal 1
cargo run --example hello_world       # terminal 2

# TCP incoming stream (Linux 6.12+; multishot accept under the hood)
cargo run --example echo_tcp_multi    # terminal 1
cargo run --example hello_world       # terminal 2

# TCP multishot accept + multishot managed recv (Linux 6.12+)
cargo run --example echo_tcp_recv_multi  # terminal 1
cargo run --example hello_world          # terminal 2

# Minimal incoming() demo (Linux 6.12+)
cargo run --example accept_multi      # terminal 1
nc 127.0.0.1 8081                     # terminal 2

# UDP
cargo run --example udp_echo          # terminal 1
cargo run --example udp_client        # terminal 2

# TLS with the checked-in test-only localhost identity
cargo run --example tls_server -- \
  ../karmaio/tests/fixtures/tls/localhost.der \
  ../karmaio/tests/fixtures/tls/localhost-key.der
cargo run --example tls_client -- \
  ../karmaio/tests/fixtures/tls/ca.der

# Use tls_split_client instead to exercise completion-native owned halves.
cargo run --example tls_split_client -- \
  ../karmaio/tests/fixtures/tls/ca.der
```

## Notes

- All `#[karmaio::main]` examples drive a single-threaded runtime for the async entrypoint.
- **echo_tcp_multi**, **echo_tcp_recv_multi**, and **accept_multi** are Linux-only (io_uring multishot accept/recv, kernel 6.12+). On other platforms they exit with a short message. Multishot SQEs are not auto-rearmed after they end.
- Managed / multishot recv uses a **per-runtime buffer pool**. Each `PooledBuf` is a lease: drop or `release()` it after use. Holding all leases without recycle can end multishot streams with `ENOBUFS`. See `karmaio::buf` pool docs. Pool size defaults to 64 × 8 KiB (`RuntimeBuilder::buffer_pool_size` / `buffer_pool_buffer_len`).
- Dropping a `JoinHandle` **detaches** the task; use `abort()` to cancel. Await (or drop) handles before dropping a manually created `Runtime` — see **spawn_tasks**.
- Dropping an in-progress I/O future (e.g. via `timeout`) detaches from the result; kernel work may still complete. See runtime docs for details.
- The **signal** example may need signals sent from another terminal (or Ctrl-C).
- **process** uses `echo` / `cat` / `ls` (Unix-style); adjust if you are on a minimal environment.
- The TLS DER identity is public test material. Never use it in a deployed service.
