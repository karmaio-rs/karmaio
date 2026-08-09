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
| **accept_multi** | Minimal `incoming()` demo on `127.0.0.1:8081`. Requires Linux 6.12+. |
| **hello_world** | TCP client; pairs with `echo_tcp` / `echo_tcp_multi` on `127.0.0.1:8080`. |
| **udp_echo** | UDP echo server on `127.0.0.1:8080`. |
| **udp_client** | UDP client; pairs with `udp_echo`. |

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

# Minimal incoming() demo (Linux 6.12+)
cargo run --example accept_multi      # terminal 1
nc 127.0.0.1 8081                     # terminal 2

# UDP
cargo run --example udp_echo          # terminal 1
cargo run --example udp_client        # terminal 2
```

## Notes

- All `#[karmaio::main]` examples drive a single-threaded runtime for the async entrypoint.
- **echo_tcp_multi** and **accept_multi** are Linux-only (`incoming()` / multishot accept, kernel 6.12+). On other platforms they exit with a short message. The multishot SQE is not auto-rearmed after it ends.
- Dropping a `JoinHandle` **detaches** the task; use `abort()` to cancel. Await (or drop) handles before dropping a manually created `Runtime` — see **spawn_tasks**.
- Dropping an in-progress I/O future (e.g. via `timeout`) detaches from the result; kernel work may still complete. See runtime docs for details.
- The **signal** example may need signals sent from another terminal (or Ctrl-C).
- **process** uses `echo` / `cat` / `ls` (Unix-style); adjust if you are on a minimal environment.
