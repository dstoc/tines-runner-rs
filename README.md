# tines-runner-rs

`tines-runner-rs` is a standalone Rust daemon that runs work assigned by a
Tines instance. The daemon polls Tines, selects the effective run settings,
sends an execution request to the configured executor, and reports logs and
status to Tines. The executor creates the workspace, checks out assigned Git
repositories, and runs the selected harness. The executor can run on the
daemon host or inside a container.

The runner supports Codex and a generic custom command harness. It makes
outbound connections to Tines and Git remotes; it does not need an inbound
connection. It does not install or update harness tools, manage a service, or
update itself.

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

The daemon account needs:

- the configured executor command on `PATH` or at its configured path;
- network access to the Tines instance;
- a Tines user API key for the first registration, if no runner credentials
  file exists.

The executor environment needs the selected harness command. When repository
checkout is enabled, it also needs Git, credentials for assigned repositories,
and network access to Git remotes. For native execution, install the command
and, when needed, Git for the daemon account. For container execution, install
them inside the container. Service managers and containers often use a
different `PATH` and credentials than an interactive shell.

## Quick start

### 1. Create the configuration file

Save `config.toml` at a path you choose. For example, on Linux or macOS, use
`$HOME/.config/tines-runner-rs/config.toml`. The runner does not search for a
configuration file. Pass its path with `--config` when you run the daemon or
`--check`.

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
`codex`. Set it to `custom` to run a configured command. The daemon expands
`~` in its paths, including `executor_cwd` and `credentials_file`. It sends the configured
`workspace_parent` to the executor, which resolves it in its own environment.
If it is omitted, the executor uses its platform default. Unknown settings
cause startup to fail.

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
tines-runner-rs --config "$HOME/.config/tines-runner-rs/config.toml" --check
unset TINES_API_KEY
```

The default credentials file remains in the platform configuration directory:

- Linux and other Unix: `${XDG_CONFIG_HOME:-~/.config}/tines-runner-rs/credentials.toml`.
- macOS: `${XDG_CONFIG_HOME:-~/Library/Application Support}/tines-runner-rs/credentials.toml`.
- Windows: `${XDG_CONFIG_HOME:-%APPDATA%}/tines-runner-rs/credentials.toml` (or `%USERPROFILE%\AppData\Roaming` if `APPDATA` is unset).

The runner uses `XDG_CONFIG_HOME` only when it is absolute. It affects the
default credentials location; it does not select the runner configuration
file. Set `[storage].credentials_file` to choose a different credentials path.
On Unix, the runner creates or repairs the file with mode `0600`. Keep it
private and use the same file when restarting the daemon. Later `--check` runs
use the saved runner token and do not need `TINES_API_KEY`:

```sh
tines-runner-rs --config "$HOME/.config/tines-runner-rs/config.toml" --check
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
executor_cwd = "/var/lib/tines-runner-rs/build"

[storage]
credentials_file = "/var/lib/tines-runner-rs/build/credentials.toml"
```

Save a second config as `/etc/tines-runner-rs/review.toml` with its own
`[runner].name`, `[runner].workspace_parent`, required
`[runner].executor_cwd`, and `[storage].credentials_file`, such as
`review-codex`, `/var/lib/tines-runner-rs/review/workspaces`,
`/var/lib/tines-runner-rs/review`, and
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
tines-runner-rs --config "$HOME/.config/tines-runner-rs/config.toml"
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
| `[runner].runner_type` | Harness type: `codex` or `custom`. Defaults to `codex`. |
| `[runner].custom_command` | Optional argv array for the custom harness. Required for each assignment resolved to `custom`. |
| `[runner].repository_checkout` | Repository materialization mode: `enabled` (default) clones working trees; `metadata_only` writes `repos.json` without cloning. |
| `[runner].workspace_parent` | Optional workspace parent inside the executor environment. If omitted, the executor uses its platform default. |
| `[runner].executor` | Argument array used to reach the executor. The runner appends `execute` and does not use a shell. Defaults to `["tines-runner-rs"]`. |
| `[runner].capabilities_executor` | Optional argument array used only for capability discovery. The runner appends `capabilities`; when unset, it uses the resolved `executor` command. |
| `[runner].executor_cwd` | Required daemon-side working directory for the executor transport process. There is no default. A relative path resolves under the daemon account's home directory. |
| `[runner].run_key_delivery` | How the daemon sends each assignment's run key to the executor transport: `request` (default) or `environment`. |
| `[runner].max_concurrent` | Maximum local assignments at once; must be greater than zero. Defaults to `1`. |
| `[runner].poll_interval_seconds` | Poll interval. Must be greater than zero; defaults to `15`. |
| `[runner].allow_remote_concurrency` | Allow Tines to change the runner's concurrency. Defaults to `false`. |
| `[storage].credentials_file` | Runner credential file path. Defaults to `credentials.toml` in the configuration directory. |
| `[storage].keep_workspaces` | Workspace retention mode: `never`, `failed`, or `always`. Defaults to `never`. |
| `[storage].keep_workspaces_for_hours` | Maximum age for retained workspaces. Defaults to `72` hours. |
| `[storage].keep_workspaces_max` | Maximum number of retained workspaces. Defaults to `20`. |

### Choose native or container execution

`runner_type` selects the harness. `executor` selects the command that starts
the execution environment. The daemon starts that command directly from
`executor_cwd`; it does not use a shell. `executor_cwd` is required, has no
implicit default, and is the daemon-side working directory for the transport
process. It does not set the workspace path inside the executor.

For native execution, keep the default executor and install
`tines-runner-rs`, Codex CLI, and Git for the daemon account. Codex and Git
credentials must also be available to that account:

```toml
[runner]
runner_type = "codex"
executor = ["tines-runner-rs"]
executor_cwd = "~"
workspace_parent = "~/.local/share/tines-runner-rs/workspaces"
```

For Docker execution, install Codex CLI, Git, and repository credentials
inside the image or executor environment. The image must also contain
`tines-runner-rs`. If you retain executor workspaces, mount persistent storage
at the configured workspace parent. This Linux example does that:

```toml
[runner]
runner_type = "codex"
executor = [
  "docker", "run", "--rm", "-i",
  "--mount", "type=bind,src=/var/lib/tines-runner-rs/workspaces,dst=/var/lib/tines-runner-rs/workspaces",
  "runner-image", "tines-runner-rs"
]
executor_cwd = "/var/lib/tines-runner-rs"
workspace_parent = "/var/lib/tines-runner-rs/workspaces"
```

Each TOML array item is one argument. For example, `--mount` and its value
are separate arguments. Podman can use the same attached argument pattern.
Keep the executor in the foreground and pass `-i` so it can read the request
from stdin and write protocol events to stdout. Do not use detached container
mode.

The executor creates one `run-<UUID>` workspace and writes the effective
repository list to `repos.json` at its root. With the default
`repository_checkout = "enabled"`, it also checks out the assigned
repositories there. Set `repository_checkout = "metadata_only"` for API-only,
checking, or review harnesses that use repository URLs and branch metadata
without reading source files. This mode does not invoke Git or require Git
credentials. Harnesses can always discover the metadata at
`{workspace}/repos.json`; no repository working-tree directories are created
in metadata-only mode.

The executor resolves `workspace_parent` in its own environment. A configured
`~` uses the executor account's home. If the setting is omitted, the executor
uses its platform default. `executor_cwd` is resolved by the daemon and is not
sent to the executor; it only sets the working directory for the transport
process.

The executor applies `keep_workspaces` to its workspace before it emits the
terminal result. A short-lived `docker run --rm` container removes files on its
own filesystem when it exits, even if the executor retained them. Mount the
workspace path to persistent storage to keep executor workspaces after the
container exits.

Capability discovery also crosses the executor boundary. The daemon invokes
`capabilities_executor` when it is set and otherwise uses the resolved
`executor` command. It appends `capabilities` and runs the command from the same
required `executor_cwd` used for assignments. The daemon caches the versioned
report for ten minutes. The report includes the built-in custom harness. The
native executor discovers Codex on the daemon account's `PATH`; a container
executor discovers Codex inside the container. Custom commands do not support
Codex model effort settings. The daemon refreshes the report before it accepts
assignments with enforced effort. An invalid report or unsupported harness or
effort causes the daemon to decline the incompatible assignment.

Use `capabilities_executor` when the normal transport needs per-run credentials
that a capability probe does not need. For example, with
`run_key_delivery = "environment"`, a Docker `executor` can pass
`TINES_API_KEY` into the container while `capabilities_executor` starts the
same image without forwarding that key:

```toml
[runner]
run_key_delivery = "environment"
executor = [
  "docker", "run", "--rm", "-i", "--env", "TINES_API_KEY",
  "--mount", "type=bind,src=/var/lib/tines-runner-rs/workspaces,dst=/var/lib/tines-runner-rs/workspaces",
  "runner-image", "tines-runner-rs"
]
capabilities_executor = [
  "docker", "run", "--rm", "-i",
  "--mount", "type=bind,src=/var/lib/tines-runner-rs/workspaces,dst=/var/lib/tines-runner-rs/workspaces",
  "runner-image", "tines-runner-rs"
]
executor_cwd = "/var/lib/tines-runner-rs"
```

Keep both commands pointed at the same effective harness environment, such as
the same image and mounts, so the probe reports the harness that assignments
will use. If an override changes `executor` to a different harness environment,
set `capabilities_executor` in a matching override to probe that environment.
Capability discovery does not receive the assignment run key, and the daemon
removes inherited Tines credentials from the probe environment.
`capabilities_executor` is also resolved per assignment through `[[override]]`
entries. Like `executor`, it is an argv array and the daemon launches it
without a shell.

Use `[[override]]` entries to change a workspace parent, repository checkout
mode, run key delivery, runner type, custom harness command, executor command,
capability discovery command, or executor working directory for matching
assignments.
Each selector is optional. Every selector in one entry must match. Names match
exactly and without regard to case. Entries apply in file order; later entries
replace only the fields they set.

```toml
# Use a container executor for matching assignments.
[[override]]
project = "Payments"
workflow = "Implementation"
state = "Ready"
workspace_parent = "/var/lib/tines-runner-rs/payments/workspaces"
executor = [
  "docker", "run", "--rm", "-i",
  "--mount", "type=bind,src=/var/lib/tines-runner-rs/payments/workspaces,dst=/var/lib/tines-runner-rs/payments/workspaces",
  "runner-image", "tines-runner-rs"
]
executor_cwd = "/var/lib/tines-runner-rs"
```

Every selector in this entry must match. The executor is an argument array,
not a shell command string. The runner appends `execute` and launches it
directly from the resolved `executor_cwd`. The executor working directory is
a daemon-side transport setting; it is not sent in the execution request.
The executor resolves `workspace_parent` in its own filesystem for the harness
workspace. The container example mounts persistent storage at that path inside
the container. A configured `~` resolves under the executor account's home.
If `workspace_parent` is omitted, the executor uses its platform default. The
executor transport must stay in the foreground for the full execution and
propagate stdin, stdout, stderr, exit status, and termination. Detached Docker
and Podman modes are unsupported. Use attached commands such as
`docker run --rm -i ...` and mount any retained executor workspace storage into
the container.

Custom harness commands are argv arrays. The runner starts them directly
without a shell and sets their working directory to the assignment workspace.
Use `{prompt_file}` for the workspace's `prompt.md` path and `{workspace}` for
the workspace path. The runner replaces these placeholders inside each argv
item. It sets `TINES_API_KEY` from the run key in the request or from its
environment when available. It sets `TINES_API_URL` from the request and adds
delivered assignment environment values. Assignment values cannot override
`PATH` or the two Tines API variables. The runner removes inherited
`TINES_RUNNER_TOKEN` and `TYPESAFE_API_KEY` values before it starts the
harness. Values marked secret use the same output redaction as Codex.

### Choose run key delivery

`[runner].run_key_delivery` controls only how the daemon sends the per-run
Tines key to the configured executor transport. The default, `request`, keeps
the key in the JSON request written to executor stdin. Set it to `environment`
to omit the key from that request and set `TINES_API_KEY` on the executor
transport process instead:

```toml
[runner]
run_key_delivery = "environment"

[[override]]
project = "Payments"
run_key_delivery = "request"
```

An executor transport can consume, transform, or forward the environment key.
For example, a container transport must choose whether to pass the host value
into the container. The bundled executor reads the request key or its own
`TINES_API_KEY` environment and gives that key to the harness. A transport can
omit the key from the inner executor environment; the executor request allows
that key to be absent, and the harness then starts without `TINES_API_KEY`.

The executor-to-harness delivery is separate from the daemon-to-transport
setting. `TINES_API_URL` remains in the request and the bundled executor sets
it for the harness in either mode. The long-lived runner token is never sent
through this setting. Environment delivery is opt-in because local process
inspection can expose environment values more readily than stdin data.

One runner registration can select different commands by project, workflow,
or state. The command must be available on the executor's `PATH` or use an
absolute path:

```toml
[runner]
runner_type = "custom"
custom_command = ["default-checks", "{prompt_file}"]

[[override]]
project = "Payments"
custom_command = ["github-checks", "--prompt-file", "{prompt_file}", "--workspace", "{workspace}"]
repository_checkout = "metadata_only"

[[override]]
state = "Review"
custom_command = ["review-checks", "{workspace}"]
```

Selectors are optional and match with the existing exact, case-insensitive
rules. Entries apply in declaration order. Each entry replaces only fields it
sets, so a later matching entry replaces the custom command when it sets one.
Custom command stdout and stderr become Tines run logs. Exit code `0`
completes the run. Other exit statuses fail it and include bounded stderr
context in the failure diagnostic.

The `wrapper` setting is no longer supported. Move transport arguments such
as Docker or Podman to `executor` and set `executor_cwd`. To use a Codex
profile command, install it as `codex` in the executor environment.

## Workspace retention

The `keep_workspaces` policy applies to the executor workspace. The executor
settles its workspace before it emits the terminal result. Set
`keep_workspaces = "failed"` to retain failed runs or `"always"` to retain all
runs. The executor prunes retained workspaces using the configured age and
count limits. It removes only workspace directories that it marked as
retained.

## Troubleshooting

- **The runner cannot find Codex or Git:** check `PATH` as the daemon account
  for native execution, and in the executor environment for container
  execution. `--check` does not test Codex authentication, Git access, or
  workspace permissions.
- **Configuration fails to load:** check the selected file path, TOML syntax,
  and that the server URL and runner name are set. The daemon and `--check`
  require `--config <path>`; the runner does not search a default location.
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
- **A private repository checkout fails:** verify the executor environment's
  Git credential helper, SSH key, host-key configuration, and network access.

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

On Unix, run `cargo test --test isolated_executor_transport` to exercise the
daemon through a separate local transport process. It covers executor working
directory overrides, environment isolation, capability discovery, protocol
streams, cancellation, and timeout without Docker or Podman. It needs Python 3
and Git, but not a live Tines instance or a container runtime.

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
- [Split-runner proposal](docs/proposals/split-runner.md) — design rationale
  for the daemon/executor boundary and local execution protocol.
- [Project proposal](docs/tines-runner-rs-proposal.md) — design, scope, and
  protocol goals.
- [Implementation issue map](docs/tines-runner-rs-issues.md) — project issue
  breakdown and dependencies.
