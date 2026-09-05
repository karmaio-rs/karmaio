# karmaio-macros

Procedural macros for the [karmaio](https://crates.io/crates/karmaio) asynchronous runtime.

This crate is an implementation detail of `karmaio` and is not intended to be used directly.

Enable the `macros` feature on `karmaio` instead:

```toml
karmaio = { version = "0.1.0-alpha.3", features = ["macros"] }
```

That exposes the `#[karmaio::main]` and `#[karmaio::test]` attribute macros.
See the [karmaio documentation](https://docs.rs/karmaio) for usage details.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
