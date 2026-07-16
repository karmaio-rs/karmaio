# karmaio Examples

This directory contains examples demonstrating the main features of the karmaio runtime.

## Running Examples

To run an example, use the following command from the workspace root:

```bash
cargo run --example <example_name>
```

Or from the examples directory:

```bash
cd examples
cargo run --example <example_name>
```

## Examples

### Networking

- **hello_world** - A simple TCP client that connects to a server and sends a message.
- **echo_tcp** - A TCP echo server that accepts connections and echoes back data.
- **udp_client** - A UDP client that sends a message to a server and receives a response.

### File I/O

- **file_operations** - Demonstrates async file operations: create, read, write, rename, and remove files.

### Timers

- **timer** - Shows how to use `sleep`, `interval`, and `timeout` for async timing operations.

### Process Management

- **process** - Demonstrates async process execution, including spawning processes, piping stdin/stdout, and capturing output.

### Signals

- **signal** - Shows how to handle Ctrl-C and Unix signals (SIGTERM, SIGHUP, etc.).

### Runtime Configuration

- **runtime_builder** - Demonstrates custom runtime configuration using the builder pattern.

## Notes

- The echo_tcp example requires a running server. Start the echo_tcp server first, then connect with the hello_world client.
- The udp_client example expects a UDP server listening on port 8080.
- The signal example demonstrates signal handling and may require sending signals from another terminal.
- All examples use the `#[karmaio::main]` attribute macro for the async entrypoint.