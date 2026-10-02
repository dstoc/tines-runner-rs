# tines-runner-rs

An independent Rust binary for running Tines assignments on a local machine.
The crate has no dependency on the Tines monorepo or `@tines/shared`.

The executable loads the runner configuration, registers or loads saved runner
credentials, and polls Tines for assignments until it is stopped. Use
`--check` to validate configuration and credentials without entering the poll
loop. Assignment execution is implemented in later steps.

## Build and run

```sh
cargo build --release
cargo run -- --version
```

The executable writes structured JSON tracing events to stderr. Set `RUST_LOG`
to change the default `info` filter.

## Development checks

Run these commands before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
