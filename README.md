# tines-runner-rs

`tines-runner-rs` is a standalone Rust daemon that runs work assigned by a
Tines instance on a local machine. It polls Tines, prepares an isolated
workspace, checks out assigned Git repositories with the machine's Git
credentials, runs the assignment with Codex, and reports logs and status to
Tines.

Codex is the only supported harness. The runner makes outbound connections to
Tines and Git remotes; it does not need an inbound connection. It does not
install or update Codex, manage a service, or update itself.

## Install

Check the [GitHub Releases page](https://github.com/dstoc/tines-runner-rs/releases)
for a Linux x86_64 archive and its checksum. Extract the archive and place
`tines-runner-rs` on the daemon account's `PATH`. Make the file executable if
needed. Check the installation:

```sh
tines-runner-rs --version
```

If no binary is available for your platform, install the current source with
Rust and Cargo:

```sh
cargo install --git https://github.com/dstoc/tines-runner-rs --locked
```

Cargo usually installs the binary in `~/.cargo/bin`; add that directory to
the daemon account's `PATH`.

## Requirements

The operating-system account that runs the daemon needs:

- Git, available on `PATH`;
- Codex CLI, available on `PATH` and authenticated for that account;
- Git credentials that can read every assigned repository;
- network access to the Tines instance and assigned Git remotes;
- a Tines user API key for the first registration, if no runner credentials
  file exists.

For private repositories, configure the daemon account's Git credential
helper or SSH key and host-key settings. Confirm that Codex and Git work as
that account. Service managers often use a different `PATH` and credentials
than an interactive shell.

## Quick start

### 1. Create the configuration file

Create `config.toml` in the runner configuration directory:

| Platform | Default configuration file |
| --- | --- |
| Linux and other Unix | `${XDG_CONFIG_HOME:-~/.config}/tines-runner-rs/config.toml` |
| macOS | `${XDG_CONFIG_HOME:-~/Library/Application Support}/tines-runner-rs/config.toml` |
| Windows | `${XDG_CONFIG_HOME:-%APPDATA%}/tines-runner-rs/config.toml` |

The runner uses `XDG_CONFIG_HOME` only when it is an absolute path. On
Windows, it uses `%USERPROFILE%\AppData\Roaming` when `APPDATA` is not set.
Pass `--config /path/to/config.toml` to select a different file. Without this
option, the runner uses the default path in the table.

Start with this configuration and replace the URL and runner name:

```toml
[server]
url = "https://tines.example.com"

[runner]
name = "workstation-codex"
runner_type = "codex"
executor = ["tines-runner-rs"]
executor_cwd = "~"
max_concurrent = 1
poll_interval_seconds = 15

[storage]
keep_workspaces = "never"
```

`[server].url` and `[runner].name` are required. The runner type defaults to
`codex`; it is the only supported type. The runner expands `~` at the start of
configured paths. Unknown settings cause startup to fail.

### 2. Register and validate

On first use, set `TINES_API_KEY` to a Tines user API key. `--check` registers
the runner, saves its runner ID and long-lived token in `credentials.toml`,
then validates the saved credentials without polling for assignments. The
user API key is not saved. In Bash or Zsh, enter it without adding it to shell
history:

```sh
printf 'Tines API key: '
IFS= read -r -s TINES_API_KEY
printf '\n'
export TINES_API_KEY
tines-runner-rs --check
unset TINES_API_KEY
```

The default credentials file is in the platform configuration directory. Set
`[storage].credentials_file` to choose another path. On Unix, the runner
creates or repairs the file with mode `0600`. Keep it private and use the same
file when restarting the daemon. Later `--check` runs use the saved runner
token and do not need `TINES_API_KEY`:

```sh
tines-runner-rs --check
```

### Run multiple runners

Give each runner its own config file, registration name, credentials file,
and workspace directory. For example, save this as
`/etc/tines-runner-rs/build.toml`:

```toml
[server]
url = "https://tines.example.com"

[runner]
name = "build-codex"
workspace_parent = "/var/lib/tines-runner-rs/build/workspaces"

[storage]
credentials_file = "/var/lib/tines-runner-rs/build/credentials.toml"
```

Save a second config as `/etc/tines-runner-rs/review.toml` with its own
`[runner].name`, `workspace_parent`, and `[storage].credentials_file`, such as
`review-codex`, `/var/lib/tines-runner-rs/review/workspaces`, and
`/var/lib/tines-runner-rs/review/credentials.toml`. Register and start each
runner with its selected file:

```sh
tines-runner-rs --config /etc/tines-runner-rs/build.toml --check
tines-runner-rs --config /etc/tines-runner-rs/review.toml --check
tines-runner-rs --config /etc/tines-runner-rs/build.toml
tines-runner-rs --config /etc/tines-runner-rs/review.toml
```

Each config keeps the runner's registration and local state separate. Run
`--check` for both files with `TINES_API_KEY` set on first registration.

### 3. Start the daemon

After `--check` succeeds, start the runner:

```sh
tines-runner-rs
```

The process polls until stopped. Use Ctrl-C in a terminal or the service
manager's normal stop command. Restart it after changing `config.toml` or
`credentials.toml`.

## Configuration

The main settings are:

| Setting | Purpose |
| --- | --- |
| `[server].url` | Tines instance URL. Required; must use HTTP or HTTPS. |
| `[runner].name` | Name used to register this runner. Required. |
| `[runner].runner_type` | Harness type. Only `codex` is supported. |
| `[runner].workspace_parent` | Parent directory for assignment workspaces. |
| `[runner].wrapper` | Optional argument array prepended to the Codex command. It is run directly, without a shell. |
| `[runner].executor` | Argument array used to reach the executor. The runner appends `execute` and does not use a shell. Defaults to `["tines-runner-rs"]`. |
| `[runner].executor_cwd` | Required working directory for the daemon-side executor process. Defaults to the daemon account's home directory. Relative paths resolve from that directory. |
| `[runner].max_concurrent` | Maximum local assignments at once; must be greater than zero. Defaults to `1`. |
| `[runner].poll_interval_seconds` | Poll interval. Must be greater than zero; defaults to `15`. |
| `[runner].allow_remote_concurrency` | Allow Tines to change the runner's concurrency. Defaults to `false`. |
| `[storage].credentials_file` | Runner credential file path. Defaults to `credentials.toml` in the configuration directory. |
| `[storage].keep_workspaces` | Workspace retention mode: `never`, `failed`, or `always`. Defaults to `never`. |
| `[storage].keep_workspaces_for_hours` | Maximum age for retained workspaces. Defaults to `72` hours. |
| `[storage].keep_workspaces_max` | Maximum number of retained workspaces. Defaults to `20`. |

Use `[[override]]` entries to change a workspace parent, runner type, wrapper,
executor command, or executor working directory for matching assignments.
Each selector is optional. Every selector in one entry must match. Names match
exactly and without regard to case. Entries apply in file order; later entries
replace only the fields they set.

```toml
# Use a separate workspace root and wrapper for matching assignments.
[[override]]
project = "Payments"
workflow = "Implementation"
state = "Ready"
workspace_parent = "~/work/payments"
wrapper = ["/usr/local/bin/codex-profile", "--name", "implementation"]
executor = ["docker", "run", "--rm", "-i", "runner-image"]
executor_cwd = "/var/lib/tines-runner-rs"
```

Every selector in this entry must match. A wrapper is an argument array, not
a shell command string. The executor working directory belongs to the daemon
transport; it is not sent in the execution request. `workspace_parent` is sent
to the executor and is interpreted in the executor environment.

## Workspace retention

The default `never` mode removes a workspace after Tines accepts the run's
terminal status. Set `keep_workspaces = "failed"` to retain failed runs or
`"always"` to retain all runs. The runner prunes retained workspaces at
startup and after each settled run, using the configured age and count limits.
It removes only workspace directories that it marked as retained.

## Troubleshooting

- **The runner cannot find Codex or Git:** check `PATH` as the daemon account.
  `--check` does not test Codex authentication, Git access, or workspace
  permissions.
- **Configuration fails to load:** check the selected file path, TOML syntax,
  and that the server URL and runner name are set. Without `--config`, the
  runner reads `config.toml` from the default configuration directory.
- **First registration fails:** confirm `TINES_API_KEY` is set for the
  `--check` process and can access the configured Tines instance. The key is
  needed only when no saved runner credentials exist.
- **Tines rejects the saved runner token:** check the configured credentials
  file and its permissions. The runner stops without falling back to
  `TINES_API_KEY`; if the token was revoked, move the credentials file aside
  and run `--check` again with a new user API key to register.
- **The runner reports a fencing conflict:** Tines has assigned the runner to
  another daemon instance. Stop duplicate processes and leave one daemon
  using the runner credentials.
- **A private repository checkout fails:** verify the daemon account's Git
  credential helper, SSH key, and host-key configuration against that remote.

The runner writes structured JSON events to stderr. Set `RUST_LOG` to change
the default `info` log filter.

## Development

Run the standard checks before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
RUST_TEST_THREADS=2 cargo test --all-targets
bash scripts/package-release.test.sh
```

On Unix, run the protocol acceptance suite with
`cargo test --test fake_tines_integration`; it needs Git and `curl`, but not a
live Tines instance.

## Releases

The release workflow targets Linux x86_64. When a tagged version is
published, its GitHub release includes a versioned `.tar.gz` archive and a
SHA256 checksum.

Release Please opens or updates a release PR from conventional commits on
`main`. Review and merge it to publish the version tag and GitHub release.
GitHub does not start CI for that generated PR because Release Please uses the
repository's default `GITHUB_TOKEN`; run **Actions → CI → Run workflow** on the
release PR's branch. CI starts automatically for ordinary pull requests.

## More documentation

- [Operator guide](docs/operation.md) — service examples and detailed setup.
- [Project proposal](docs/tines-runner-rs-proposal.md) — design, scope, and
  protocol goals.
- [Implementation issue map](docs/tines-runner-rs-issues.md) — project issue
  breakdown and dependencies.
