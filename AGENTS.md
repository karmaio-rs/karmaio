# AGENTS.md - Developer Guidelines for karmaio

## Project Overview

karmaio is a modern, fast, completion based multi-threaded share-nothing asynchronous runtime for Rust, using io-uring on Linux, IOCP on Windows and Kqueue on macOS.

## Package Management, Build, Lint, and Test Commands

This is a standard rust project with cargo. All the standard rust/cargo commands and patterns will work.

## Code Style Guidelines

### Naming Conventions

- **Traits**: `AsyncRead`, `AsyncWrite`, `Driver` - PascalCase, descriptive
- **Structs/Enums**: `Task<S>`, `Op<T>` - PascalCase
- **Functions/Methods**: `from_raw`, `schedule`, `run` - snake_case
- **Variables**: `task_ptr`, `raw`, `buf` - snake_case
- **Constants**: SCREAMING_SNAKE_CASE
- **Generic Type Parameters**: Single uppercase letter `T`, `S`, `F`, `B`

In case the above guidelines don't cover something, default to standard rust conventions.

### Visibility

- Use `pub(crate)` for module-level public items
- Use `pub` only for truly public API
- Use `pub(super)` for parent-visible items
- Keep implementation details private

### Async/Await

- Use `async fn` for async functions instead of `impl Future`
- Use `.await` directly without additional wrapping

### Error Handling

- Use `std::io::Result<T>` for std I/O operations
- Use `(std::io::Result<T>, B)` tuples for buffer-based I/O operations (`BufResult`)
- Propagate errors with `?` operator
- Document error conditions in doc comments

### Unsafe Code

- Document all unsafe blocks with clear justification
- Use `unsafe fn` only when necessary
- Prefer safe abstractions over unsafe when possible
- Comment memory safety invariants

### Traits and trait implementations

- Add comments to public traits and functions explaining purpose and contract
- Use `#[inline]` on simple trait implementations

### Documentation

- Add comments (`//`) to public traits, structs, functions
- Use complete sentences in documentation
- Document `unsafe` preconditions
- Example from codebase:
```rust
// A customized result that returns both the result of the operation and the buffer used for it.
// This is needed because `io-uring` needs full ownership of the buffer
pub type BufResult<T, B> = (std::io::Result<T>, B);
```

### Testing

- Use `#[test]` attribute for unit tests
- Group tests in the same module or in `tests/` directory
- Use descriptive test names: `test_name_describes_behavior`

### Module Organization

- Use `pub mod` for public modules
- Use `pub(crate) mod` for crate-internal modules
- Use `mod` for private modules
- Follow module hierarchy in file structure
  - Do not use the `mod.rs` file for for the module entry point
  - Instead use a `<module>.rs` file in the parent module

---
