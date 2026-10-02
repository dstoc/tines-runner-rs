# tines-runner-rs

An independent Rust binary for running Tines assignments on a local machine.
The crate has no dependency on the Tines monorepo or `@tines/shared`.

The executable loads the runner configuration, registers or loads saved runner
credentials, and polls Tines for assignments until it is stopped. Use
`--check` to validate configuration and credentials without entering the poll
loop. The runner executes assignments in isolated workspaces using Codex.

## Build and run

```sh
cargo build --release
cargo run -- --version
```

The executable writes structured JSON tracing events to stderr. Set `RUST_LOG`
to change the default `info` filter.

## Workspace retention

By default, the runner removes a workspace after Tines accepts the run's
terminal status. Set a retention mode in `config.toml` to keep workspaces for
debugging:

```toml
[storage]
keep_workspaces = "failed" # "never", "failed", or "always"
keep_workspaces_for_hours = 72
keep_workspaces_max = 20
```

The runner writes a `.tines-runner-retained.json` marker after it reports the
terminal status. The marker records the run ID, issue reference when known,
terminal status, failure reason when applicable, and retention time. The age
and count limits prune on startup and after each settled run. Pruning only
deletes workspace directories with a valid marker. The runner leaves unmarked
directories untouched during pruning.

## Development checks

Run these commands before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
