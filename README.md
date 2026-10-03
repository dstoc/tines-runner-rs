# tines-runner-rs

An independent Rust binary for running Tines assignments on a local machine.
The crate has no dependency on the Tines monorepo or `@tines/shared`.

The executable loads the runner configuration, registers or loads saved runner
credentials, and polls Tines for assignments until it is stopped. Use
`--check` to validate configuration and credentials without entering the poll
loop. The runner executes assignments in isolated workspaces using Codex and
reports Codex output and run status to Tines.

For installation, configuration, first-run registration, service examples, and
troubleshooting, see the [operator guide](docs/operation.md).

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
and count limits prune on startup and after each settled run. Startup pruning
checks the default workspace parent and every workspace parent set by an
override. Pruning only deletes workspace directories with a valid marker. The
runner leaves unmarked directories untouched during pruning.

## Protocol acceptance

Run the end-to-end protocol acceptance suite on Unix with:

```sh
cargo test --test fake_tines_integration
```

The suite starts an isolated Tines protocol server and a stub Codex executable.
The acceptance scenario registers a local runner, routes a test issue to its
name, checks the project/workflow/state override, clones a local Git fixture
into the assignment workspace, and uses the run key from the Codex process to
read issue details. It also checks log streaming and the completed finish
state. Other scenarios cover retries, rate limits, cancellation, timeout,
concurrency, crash recovery, graceful shutdown, and daemon fencing/reconnect.
The check needs Git and curl. It does not need a live Tines deployment or a
provider account.

## Development checks

Run these commands before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
RUST_TEST_THREADS=2 cargo test --all-targets
```
