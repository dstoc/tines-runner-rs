# Operating tines-runner-rs

This guide covers installation, setup, normal operation, and common failures.
The runner supports the `codex` harness.

## How execution is split

The daemon polls Tines, resolves assignment settings, sends an execution
request to the configured executor, and sends logs and the terminal status to
Tines. The executor creates the workspace, checks out the assigned
repositories, and runs the selected harness. The executor can be the local
runner binary or a command that starts a container.
`runner_type` selects the harness; `executor` selects the transport command.

For design rationale and the local executor protocol, see the
[split-runner proposal](proposals/split-runner.md).

## Requirements

Install the configured executor command for the operating-system account that
runs the daemon. That account also needs:

- `tines-runner-rs`;
- network access to the Tines server.

The executor environment needs Codex, Git, credentials for assigned
repositories, and network access to Git remotes. With the native executor,
that is the daemon account. With a container executor, configure these inside
the container. Keep the container command on the daemon account's `PATH`, and
install `tines-runner-rs` in the executor environment. Service managers and
containers may use different `PATH` values and credentials than an interactive
terminal.

Check native commands as the service account:

```sh
command -v codex
codex --version
command -v git
git --version
```

For native execution, the runner uses the service account's Codex configuration
and Git credentials. For private repositories with container execution,
configure Git credentials inside the executor. Confirm access to each remote
from the executor environment before starting the daemon. The runner makes
outbound requests; it does not require inbound network access.

With a container or another isolated executor, install Codex and Git and
configure repository credentials inside that executor environment as well.
The executor runs `tines-runner-rs execute`; `runner_type` selects the semantic
harness, while `executor` selects the local command that starts the executor.

The runner discovers capabilities by invoking the configured executor command
with `capabilities`. The executor writes one versioned JSON document to stdout.
The document reports semantic harness support, harness versions, and effort
catalogs. The runner caches it for ten minutes and refreshes it before it
processes an assignment with enforced effort.

The default executor uses the local runner binary, so Codex must be available
in the daemon account's `PATH`. A container executor discovers Codex inside the
container. If the executor fails, returns malformed data, or cannot verify the
configured harness or requested effort, the runner declines incompatible
assignments. Tines receives the verified effort catalog and can route only a
model and effort pair that the executor reports as supported.

## Install the binary

The repository provides a Cargo-built executable. Install it from the source
repository with Rust and Cargo:

```sh
cargo install --git https://github.com/dstoc/tines-runner-rs --locked
```

Cargo installs `tines-runner-rs` in its binary directory, usually
`~/.cargo/bin`. Add that directory to the service account's `PATH`, or set the
full path in the service definition. You can also build a release binary with
`cargo build --release` and deploy `target/release/tines-runner-rs` to a
location on the service account's `PATH`. These are Cargo and manual binary
deployment paths. The runner has no built-in installer or updater.

Check that the executable starts:

```sh
tines-runner-rs --version
```

Keep the binary at a stable path when a service manager runs it. Stop the
daemon before replacing the binary. Operators manage runner updates by
installing or deploying a new binary and restarting the service.

## Configure the runner

Save the runner configuration at a path you choose, such as
`/etc/tines-runner-rs/config.toml`. The runner does not search for a
configuration file. Pass its path with `--config` when you run the daemon or
`--check`.

The default workspace directories are:

| Platform | Workspace directory |
| --- | --- |
| Linux and other Unix systems | `${XDG_DATA_HOME:-~/.local/share}/tines-runner-rs/workspaces` |
| macOS | `${XDG_DATA_HOME:-~/Library/Application Support}/tines-runner-rs/workspaces` |
| Windows | `${XDG_DATA_HOME:-%LOCALAPPDATA%}/tines-runner-rs/workspaces` |

The workspace directory defaults shown in the table apply in the executor
environment. On Windows, if `APPDATA` or `LOCALAPPDATA` is not set, the
executor uses the equivalent directories under `%USERPROFILE%\AppData\Roaming`
and `%USERPROFILE%\AppData\Local`. XDG directory variables are used only when
they contain absolute paths.

The following example shows the supported settings and their defaults:

```toml
[server]
url = "https://tines.tbuckley.dev"

[runner]
name = "workstation-codex"
runner_type = "codex"
workspace_parent = "~/.local/share/tines-runner-rs/workspaces"
executor = ["tines-runner-rs"]
executor_cwd = "~"
max_concurrent = 1
allow_remote_concurrency = false
poll_interval_seconds = 15

[storage]
credentials_file = "~/.config/tines-runner-rs/credentials.toml"
keep_workspaces = "never"
keep_workspaces_for_hours = 72
keep_workspaces_max = 20
```

`[server].url`, `[runner].name`, and `[runner].executor_cwd` are required. The
server URL must use HTTP or HTTPS. The only supported `runner_type` is
`codex`. `executor_cwd` has no default. The runner expands `~` and resolves a
relative path under the daemon account's home directory. The runner registers
with Tines using the configured name and concurrency. Tines and this local
configuration must agree about which work the runner can accept.

`max_concurrent` must be between 1 and 100. `poll_interval_seconds` must be
greater than zero. A configured workspace parent must be writable in the
executor environment.

### Choose an executor environment

Set `[runner].executor_cwd` in the base configuration. It is required and has
no implicit default. An override can replace its value for matching
assignments. The daemon expands `~` and resolves a relative value under the
daemon account's home directory, then uses that path as the working directory
when it starts the executor transport process. The path must exist and be
accessible to that account.

For native execution, the default executor starts the local runner binary.
Install `tines-runner-rs`, Codex CLI, and Git for the daemon account, and make
Codex and Git credentials available to that account:

```toml
[runner]
runner_type = "codex"
executor = ["tines-runner-rs"]
executor_cwd = "~"
workspace_parent = "~/.local/share/tines-runner-rs/workspaces"
```

For container execution, install `tines-runner-rs`, Codex CLI, and Git inside
the image or executor environment. Configure Codex and repository credentials
there as well. If you retain executor workspaces, mount persistent storage at
the workspace parent, as in this example:

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

Each TOML array item is one argument. The daemon passes the arguments directly
to the process; it does not parse shell quoting or run a shell. The daemon
appends the executor mode (`capabilities` or `execute`) to the configured
arguments. The transport must stay in the foreground, accept stdin, and
propagate stdout, stderr, exit status, and termination. Podman can use the
same attached argument pattern. Detached container modes are not supported.

The daemon resolves `executor_cwd` on the host and uses it only as the
executor transport process's working directory. It does not send this path to
the executor. The request carries the configured `workspace_parent` and
retention settings. The executor resolves that value in its own filesystem
and creates one `run-<UUID>` workspace there. A configured `~` resolves under
the executor account's home. If `workspace_parent` is omitted, the executor
uses its platform default. The daemon expands `~` in its own paths, including
`executor_cwd` and `credentials_file`. Unknown configuration keys cause
startup to fail.

### Configure execution overrides

An override can select a different workspace parent, executor command, or
executor working directory for assignments that match its project, workflow,
and state names:

```toml
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

Each selector is optional. All selectors in one entry must match. Name
matching is exact and case-insensitive. The runner applies matching entries in
file order; a later entry replaces earlier values only for fields it sets.
Overrides can set `workspace_parent`, `runner_type`, `executor`, or
`executor_cwd`. Executor entries are arguments, not a shell command string.
The runner appends `execute` and starts the command from the resolved
`executor_cwd`. The executor transport must stay in the foreground for the full
execution and propagate stdin, stdout, stderr, exit status, and termination.
Detached Docker and Podman modes are unsupported; use attached commands such as
`docker run --rm -i ...`.

The `wrapper` setting is no longer supported. Move transport arguments such
as Docker or Podman into `executor` and set `executor_cwd`. To use a Codex
profile command, install it as `codex` in the executor environment.

The runner resolves the workflow name with an extra request for each
assignment: `GET /api/v1/issues/{issue_id}`, authenticated with that
assignment's run key. Tines currently provides the workflow name in the issue
detail response, not in the assignment. The runner combines that current
workflow name with the project name and the immutable state-at-start name from
the assignment. If someone changes the issue's workflow between dispatch and
the lookup, the workflow name can reflect the later change.

The Codex process receives the assignment run key as `TINES_API_KEY` and the
server URL as `TINES_API_URL`. These values are separate from the user API key
used to register the runner.

## Register and start

On the first start, the runner needs a Tines user API key in `TINES_API_KEY`.
It uses this key to register the configured runner, then saves the returned
runner ID and long-lived runner token in `credentials.toml`. The user API key
is not saved. Do not put it in `config.toml`, a service definition, or a
command-line argument.

Run the first check as the same operating-system account that will run the
daemon. The following Bash or Zsh commands prompt for the key without adding
it to shell history:

```sh
printf 'Tines API key: '
IFS= read -r -s TINES_API_KEY
printf '\n'
export TINES_API_KEY
tines-runner-rs --config /etc/tines-runner-rs/config.toml --check
unset TINES_API_KEY
```

On a first start, `--check` registers the runner and writes the credentials
file, then validates the saved token. It exits without polling for work. On
later starts, `--check` validates the existing token and does not need
`TINES_API_KEY`. If the check succeeds, start the daemon:

```sh
tines-runner-rs --config /etc/tines-runner-rs/config.toml
```

The daemon reads its configuration and credentials at startup. Restart it
after you change either file. Stop a foreground daemon with Ctrl-C. Service
managers can stop it with their normal termination signal; the runner handles
graceful shutdown and reports that it is draining.

`--check` validates configuration and Tines credentials. It does not test
Codex authentication, Codex capability discovery, Git access, or workspace
permissions. Test those as the service account before assigning production
work.

### Credentials file

The default credentials path is `credentials.toml` in the platform
configuration directory: `${XDG_CONFIG_HOME:-~/.config}` on Linux and other
Unix systems, `${XDG_CONFIG_HOME:-~/Library/Application Support}` on macOS,
and `${XDG_CONFIG_HOME:-%APPDATA%}` on Windows. If Windows `APPDATA` is unset,
the runner uses `%USERPROFILE%\AppData\Roaming`. `XDG_CONFIG_HOME` affects
this credentials path only when it is absolute; it does not select the runner
configuration file. Set `[storage].credentials_file` to use another path. The
runner creates this file after registration. Its contents have this form:

```toml
runner_id = "rnr_example"
runner_token = "secret runner token"
```

Treat the file as a secret. On Unix, the runner creates or repairs its mode to
`0600`. Keep the file under the same service account and use the same file
when restarting the runner. Do not copy its token into logs or issue comments.

Once this file exists, the daemon authenticates with the saved runner token.
If Tines rejects that token, the runner exits. It does not replace the token or
fall back to `TINES_API_KEY`.

## Optional service examples

These examples show service-manager configuration only. The runner does not
install, enable, or manage a service. Replace the example account, paths, and
`PATH` entries with values for the host. Include the Codex and Git executable
directories in `PATH`.

### systemd

Save a unit such as `/etc/systemd/system/tines-runner-rs.service`:

```ini
[Unit]
Description=Tines Rust runner
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=tines-runner
Environment=HOME=/home/tines-runner
Environment=PATH=/home/tines-runner/.cargo/bin:/home/tines-runner/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/home/tines-runner/.cargo/bin/tines-runner-rs --config /etc/tines-runner-rs/config.toml
Restart=on-abnormal
RestartSec=5

[Install]
WantedBy=multi-user.target
```

After saving the unit, reload unit files and enable or start
`tines-runner-rs.service` with the host's normal systemd commands. Run the
first registration check interactively as `tines-runner` before enabling the
service. systemd captures the runner's JSON logs in the journal. This example
restarts after abnormal process termination, but leaves ordinary error exits
stopped for review. Resolve a fencing error before restarting the service.

For a system unit, the usual commands are:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now tines-runner-rs.service
sudo journalctl -u tines-runner-rs.service -f
```

### launchd

Save a LaunchAgent plist under
`~/Library/LaunchAgents/dev.tines.runner-rs.plist`. Create the log directory
before loading it. Replace `/Users/tines-runner` and the executable paths:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.tines.runner-rs</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/tines-runner/.cargo/bin/tines-runner-rs</string>
    <string>--config</string>
    <string>/Users/tines-runner/.config/tines-runner-rs/config.toml</string>
  </array>
  <key>WorkingDirectory</key>
  <string>/Users/tines-runner</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>/Users/tines-runner</string>
    <key>PATH</key>
    <string>/Users/tines-runner/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/Users/tines-runner/Library/Logs/tines-runner-rs.out.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/tines-runner/Library/Logs/tines-runner-rs.err.log</string>
</dict>
</plist>
```

Load and stop the LaunchAgent with the normal `launchctl` commands for the
logged-in user's GUI session. Run the first registration check in that user's
environment before loading the agent. This example starts at login but does
not automatically restart after an error. Review a fencing error before you
start the agent again.

## Workspace retention

The `keep_workspaces` policy applies to the executor workspace. The executor
settles its workspace before it emits the terminal result. Set
`keep_workspaces` to retain workspaces for debugging:

| Value | Workspaces retained |
| --- | --- |
| `"never"` | None. This is the default. |
| `"failed"` | Failed runs only. |
| `"always"` | All runs with an accepted terminal status. |

The defaults are 72 hours and 20 retained workspaces. The executor prunes
expired or over-limit workspaces. It only deletes direct child directories
that contain a valid runner retention marker. It leaves unmarked directories
alone. Set `keep_workspaces_for_hours` or `keep_workspaces_max` to change the
limits. A maximum of zero removes all marked workspaces during pruning.

A short-lived container started with `--rm` loses files from its own
filesystem when it exits, even if the executor retained them at settlement.
Mount the executor workspace path to persistent storage, as in the example
above, or use a persistent executor environment to keep retained workspaces
available for later review.

Retention is for debugging. It does not resume an interrupted assignment or
preserve state for Tines to resume. Cancellation cleanup can remove a workspace
without retaining it.

## Logs and troubleshooting

The runner writes structured JSON events to stderr. The default log level is
`info`. Use the service manager's journal or configured stderr destination to
read daemon logs. Assignment logs include executor output when Tines accepts
those log requests.

### Configuration or credential file errors

- Confirm that the daemon's `HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME`
  match the credential and workspace locations used during setup.
- Confirm that the file passed to `--config` exists and contains both required
  values: `[server].url` and `[runner].name`.
- Daemon and `--check` commands require `--config <path>`. The runner does not
  search a default configuration location.
- Check TOML syntax and field names. Unknown fields are errors.
- If the error names `credentials.toml`, verify `[storage].credentials_file`
  and confirm that the daemon account can read and write that path.

### Tines rejects the runner token

The error says `Tines rejected the runner token for runner ...`. Check that the
daemon is reading the intended credentials file and that the server URL points
to the Tines instance where the runner was registered. A bootstrap user key
does not repair or replace a rejected saved token.

If the saved token is invalid, follow the Tines runner lifecycle used by your
organization before registering again. Back up or remove the invalid
credentials file only when you intend to register a runner again. Then set a
valid user key in `TINES_API_KEY` and run
`tines-runner-rs --config /etc/tines-runner-rs/config.toml --check` as the
service account. A missing credentials file triggers registration and writes
the new runner credentials. Do not leave both old and new daemon instances
using the same runner identity.

### Tines fences this daemon

The error says `Tines superseded this daemon for runner ...`. Another daemon
has taken ownership of that runner identity. Check for a second service or a
copy running on another host with the same credentials. Stop the unintended
instance, then start the one that should own the runner. Run one active daemon
per credentials file unless you intend Tines to replace the earlier daemon.

### Assignments fail before Codex starts

- Check that the executor environment can run `git --version` and access each
  assigned repository remote using its Git credentials.
- For native execution, check that the daemon account can run `codex --version`
  and has valid Codex authentication. For container execution, check the same
  inside the container.
- Check the daemon account's `PATH` for the executor command. In the executor
  environment, check write access to the workspace parent.
- Read the assignment log for issue-detail lookup or repository clone errors.
  Workflow resolution makes an extra authenticated issue-detail request
  before the executor starts workspace preparation.

## Operational boundaries

- Resume is future work. Retained workspaces are for debugging only.
- Daemon auto-update is out of scope. Operators deploy runner updates.
- Service installation is out of scope. The unit and plist above are examples.
- Codex updates are operator-managed. Keep a compatible Codex executable
  installed and available in the service account's `PATH`.
