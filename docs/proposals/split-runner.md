# Proposal: Split runner orchestration from execution

## Summary

Refactor `tines-runner-rs` into two logical halves:

1. a **runner daemon** that communicates with Tines, owns runner registration and assignment lifecycle, and launches executions; and
2. an **executor** that consumes one self-contained execution request, prepares the workspace, runs the selected harness, and emits a versioned JSONL event stream.

Both halves will initially remain modes of the same `tines-runner-rs` binary.

The daemon will invoke the executor through a configurable argv-based `executor` command. This command is the isolation/customization boundary and may launch the executor directly, through Docker or Podman, through another sandbox, or through another local execution mechanism.

For example:

```text
Tines
  │
  │ runner protocol
  ▼
tines-runner-rs daemon
  │
  │ ExecutionRequest JSON on stdin
  ▼
configured executor command
  │
  │ e.g. docker run ...
  ▼
tines-runner-rs execute
  │
  ├─ materialize workspace
  ├─ materialize skills
  ├─ clone repositories
  ├─ launch Codex
  ├─ parse Codex JSONL
  └─ emit generic execution JSONL
  │
  ▼
tines-runner-rs daemon
  │
  ├─ append Tines run logs
  └─ report Tines run completion
```

This separates Tines orchestration from the environment in which untrusted or tool-heavy agent work executes.

It also creates a stable harness abstraction for future support for Claude Code and other execution harnesses.

## Motivation

The current runner executes the complete run lifecycle in one process:

- poll Tines;
- receive an assignment;
- resolve local configuration;
- create the workspace;
- clone repositories;
- construct a Codex command;
- optionally prepend a wrapper;
- supervise Codex;
- parse Codex output;
- stream logs;
- report completion.

The existing `wrapper` mechanism is useful for simple command decoration, but in practice it may represent a much stronger boundary.

For example, an operator may use the wrapper to execute Codex inside a container. In that configuration, workspace creation, repository cloning, Codex installation, provider credentials, and process supervision logically belong inside the container as well.

Treating the container command as merely a wrapper around Codex leaves responsibility split awkwardly across the isolation boundary.

Instead, the isolation boundary should be around the entire execution of one Tines assignment.

The outer process should answer:

> What work should run, and what does Tines need to know about it?

The inner process should answer:

> How is this assignment actually executed in this environment?

## Goals

This change should:

- separate Tines runner orchestration from assignment execution;
- permit the executor to run inside a container or other isolation mechanism;
- pass the assignment to the executor without placing secrets or large JSON values in argv;
- keep Tines runner credentials outside the execution environment;
- pass only the ephemeral run credential into the executor;
- move workspace creation and repository cloning into the executor;
- move harness invocation and harness-specific parsing into the executor;
- define a stable, versioned JSONL protocol from executor to daemon;
- keep Tines run-log sequencing, retries, and finish reporting in the daemon;
- preserve local project/workflow/state execution overrides;
- preserve a configurable execution command so operators can customize container execution;
- retain current native, non-container execution as the simplest configuration;
- make future harnesses possible without coupling the daemon to their output formats;
- avoid requiring a Tines server-side protocol change.

## Non-goals

The initial split will not:

- change Tines assignment scheduling;
- require a new Tines execution-context endpoint;
- implement remote execution over the network;
- make the executor a persistent service;
- expose runner registration credentials to the executor;
- move Tines log sequencing or run settlement into the executor;
- implement resume;
- require multiple binaries or crates;
- require support for multiple harnesses in the first implementation;
- define arbitrary server-controlled executable paths.

The daemon and executor may be separated into distinct binaries later if that becomes useful, but it is not required by this proposal.

## Terminology

### Daemon

The long-running outer process.

It registers with Tines, polls for assignments, reconciles ownership, handles cancellation, and reports results.

Conceptually:

```text
tines-runner-rs daemon
```

The existing no-subcommand invocation may remain an alias for daemon mode for compatibility.

### Executor

A short-lived process responsible for exactly one assignment.

Conceptually:

```text
tines-runner-rs execute
```

It reads one execution request from stdin and writes execution events as JSONL to stdout.

### Harness

The agent-facing program used to perform the assignment.

Initially:

```text
codex
```

Future examples may include:

```text
claude_code
custom
```

The executor selects an internal harness adapter from the semantic harness identifier. The assignment does not need to specify the machine-specific executable path.

### Executor command

The locally configured argv used by the daemon to reach the executor environment.

For native execution it may effectively be:

```text
tines-runner-rs
```

and the daemon appends:

```text
execute
```

For container execution it might be:

```toml
executor = [
  "docker",
  "run",
  "--rm",
  "-i",
  "ghcr.io/example/tines-runner-rs",
  "tines-runner-rs"
]
```

The daemon then directly spawns:

```text
docker run --rm -i \
  ghcr.io/example/tines-runner-rs \
  tines-runner-rs execute
```

No intermediate shell is used.

If the image defines `tines-runner-rs` as its entrypoint, the configuration may be shorter.

## Responsibility split

### Runner daemon responsibilities

The daemon owns:

- configuration loading;
- stored runner credentials;
- runner registration and reconnect;
- polling;
- daemon instance fencing;
- `owned_runs`;
- Tines-side concurrency reconciliation;
- assignment acceptance and decline;
- cancellation reception;
- project/workflow/state lookup needed by local overrides;
- local override resolution;
- selection of the executor command;
- selection of the semantic harness;
- executor process supervision;
- Tines run-log batching;
- Tines run-log sequence numbers;
- log retry and idempotency;
- Tines run finish;
- graceful daemon shutdown;
- active execution tracking and crash recovery.

The daemon does not need Codex installed when the configured executor environment provides Codex.

### Executor responsibilities

The executor owns:

- decoding and validating one execution request;
- workspace creation;
- prompt materialization;
- repository metadata materialization;
- skill materialization;
- repository cloning;
- delivery of assignment environment values;
- exposure of the run key to the harness;
- harness command construction;
- harness process supervision;
- assignment timeout enforcement;
- harness-specific JSON/event parsing;
- session/thread extraction;
- usage extraction;
- provider rate-limit detection;
- workspace retention and cleanup;
- translation of harness-specific output into generic executor events.

The executor does not:

- register as a Tines runner;
- possess the long-lived runner token;
- poll for work;
- append Tines run-log sequence numbers;
- retry Tines log requests;
- finish the run directly through the runner protocol.

## Execution request

The daemon sends exactly one JSON document to the executor's stdin.

The request is a local protocol between the two halves of `tines-runner-rs`. It is distinct from the Tines runner protocol and should be explicitly versioned.

A representative shape is:

```json
{
  "version": 1,
  "tines": {
    "api_url": "https://tines.example.test"
  },
  "execution": {
    "harness": "codex",
    "workspace_parent": "/workspace",
    "retention": {
      "mode": "never",
      "max_age_hours": 72,
      "max_count": 20
    }
  },
  "assignment": {
    "run": {
      "id": "arun_...",
      "issue_id": "iss_...",
      "model": "gpt-5.6-codex",
      "issue_ref": {
        "project_name": "Example",
        "number": 42,
        "title": "Implement feature"
      },
      "state_at_start_name": "Implement"
    },
    "effort": {
      "version": 1,
      "value": "high"
    },
    "prompt": "Implement the assigned issue.",
    "bundle": {
      "skills": [],
      "repos": []
    },
    "run_key": "...",
    "env": [],
    "timeout_minutes": 120
  }
}
```

The exact schema should reuse existing protocol structures where practical, but the outer `ExecutionRequest` should remain a distinct type so the local protocol can evolve independently.

### Why stdin

The execution request may contain:

- large prompt/context content;
- skills;
- repository metadata;
- environment values;
- the ephemeral run key.

It must therefore not be passed as a command-line argument.

Using stdin:

- avoids argv size limits;
- avoids JSON escaping problems;
- prevents the run key and assignment secrets from appearing in process command lines;
- works naturally with `docker run -i`;
- permits assignments substantially larger than a practical argv payload.

The daemon closes the executor's stdin once the complete request has been written.

## Harness selection

The execution request carries a semantic harness identifier:

```json
{
  "harness": "codex"
}
```

The executor maps this identifier to an internal adapter.

For example:

```text
codex
  ↓
CodexAdapter
  ├─ construct codex argv
  ├─ parse Codex JSONL
  ├─ identify Codex session
  ├─ extract Codex usage
  └─ recognize Codex rate limits
```

Future adapters could provide equivalent behavior for other harnesses.

The request should not ordinarily contain:

```text
/usr/local/bin/codex
```

or another host-specific executable path.

Executable location and container contents are properties of the executor environment.

This keeps Tines scheduling data semantic rather than machine-specific.

## Executor command configuration

The current `wrapper` concept should evolve into an `executor` concept.

For example:

```toml
[runner]
executor = ["tines-runner-rs"]
```

or:

```toml
[runner]
executor = [
  "docker",
  "run",
  "--rm",
  "-i",
  "--init",
  "ghcr.io/example/tines-runner-rs",
  "tines-runner-rs"
]
```

The daemon appends the required executor subcommand.

For a run:

```text
<executor argv...> execute
```

The executor remains eligible for project/workflow/state overrides:

```toml
[[override]]
project = "sensitive-project"
executor = [
  "docker",
  "run",
  "--rm",
  "-i",
  "--network=none",
  "isolated-runner",
  "tines-runner-rs"
]
```

This retains the policy mechanism currently served by `wrapper`, but applies it at the correct isolation boundary.

A compatibility period may continue accepting `wrapper` as a deprecated configuration name if migration cost warrants it.

The implementation must not use a shell to interpret executor argv.

## Workspace configuration

Because the executor owns materialization, workspace paths are interpreted in the executor environment.

This is important for container execution.

For example:

```toml
workspace_parent = "/workspaces"
```

means `/workspaces` inside the configured executor environment, not necessarily a path on the daemon host.

The daemon should pass the configured workspace policy to the executor without resolving executor-local filesystem semantics prematurely.

For direct/native execution, this preserves the existing behavior conceptually.

For container execution, operators may mount a persistent host directory if retained workspaces are desired.

For an ephemeral `--rm` container with no persistent volume, workspace retention naturally lasts only for the container lifetime.

## Executor output protocol

Executor stdout is reserved exclusively for a versioned JSONL protocol.

Each line is one complete JSON object.

The daemon parses these events and translates them into Tines runner-protocol actions.

Harness-native output must not pass through stdout unwrapped.

### Log event

```json
{
  "version": 1,
  "type": "log",
  "stream": "stdout",
  "message": "Cloning repository..."
}
```

A harness adapter can emit readable normalized log messages regardless of the harness's native format.

### Session event

```json
{
  "version": 1,
  "type": "session",
  "provider": "codex",
  "id": "thread_..."
}
```

### Usage event

```json
{
  "version": 1,
  "type": "usage",
  "input_tokens": 1200,
  "output_tokens": 400,
  "cache_read_tokens": 800,
  "cache_write_tokens": 0
}
```

Usage events may update the latest observed usage during execution.

### Rate-limit event

```json
{
  "version": 1,
  "type": "rate_limit",
  "resume_at": "2026-10-04T05:30:00Z"
}
```

### Terminal result

Every ordinarily terminating execution should emit exactly one terminal result.

Successful example:

```json
{
  "version": 1,
  "type": "result",
  "status": "completed",
  "exit_code": 0
}
```

Failure example:

```json
{
  "version": 1,
  "type": "result",
  "status": "failed",
  "exit_code": 1,
  "error": "Codex exited with status 1"
}
```

Rate-limited example:

```json
{
  "version": 1,
  "type": "result",
  "status": "rate_limited",
  "resume_at": "2026-10-04T05:30:00Z"
}
```

The precise final schema should support all information currently required by Tines finish reporting, including:

- terminal status;
- error;
- provider session/thread ID;
- usage;
- pricing evidence;
- rate-limit/reset information;
- interruption where locally appropriate.

The final result may repeat the latest session and usage values so the terminal record is self-contained.

## Protocol rules

The executor protocol should follow these rules:

- every stdout line is valid JSON;
- every event carries a protocol version;
- unknown additive event types may be ignored by a compatible daemon;
- secrets must never be emitted;
- the run key must never be emitted;
- assignment environment secrets must never be emitted;
- stdout that is not valid protocol JSON is a protocol error;
- event line sizes should be bounded to protect the daemon from accidental or malicious unbounded output;
- exactly one terminal `result` is expected during ordinary completion;
- EOF without a terminal result is an executor failure;
- a terminal result followed by further output is a protocol violation;
- executor stderr is reserved for local executor diagnostics and is not itself the machine protocol.

The daemon may include a bounded amount of executor stderr in an operator-facing error if the executor fails before producing a terminal result, provided secret redaction remains effective.

## Tines log delivery

The executor does not call the Tines run-log endpoint.

Instead:

```text
harness output
      ↓
harness adapter
      ↓
executor log event
      ↓
daemon
      ↓
Tines log batching + seq
```

This keeps the existing Tines idempotency and sequencing model in exactly one place.

If communication with Tines temporarily fails, the daemon can continue consuming executor events while buffering according to existing bounded-log behavior.

The executor does not need to understand Tines log sequence numbers or retry semantics.

## Run completion

The executor reports execution outcome to the daemon.

The daemon remains authoritative for deciding whether a Tines `finish` request is appropriate.

This distinction is important because Tines supervisor cancellation has different semantics from ordinary harness failure.

For ordinary completion:

```text
executor result
    ↓
daemon
    ↓
POST finish
```

For supervisor cancellation:

```text
Tines cancel
    ↓
daemon terminates executor
    ↓
no duplicate finish
```

The executor therefore never needs to know whether Tines has already settled a canceled run.

## Cancellation

Tines cancellation remains owned by the daemon.

When the daemon receives a cancellation for an active run it should:

1. stop accepting further executor output for Tines delivery as required by current cancellation semantics;
2. terminate the executor process group;
3. escalate to forced termination after a bounded grace period if required;
4. acknowledge cancellation according to the runner protocol;
5. avoid sending a second finish for a run already settled by Tines.

The configured executor command is responsible for providing useful process-tree semantics.

For container execution, the configured command should propagate termination into the container. Operators should use container options appropriate for reliable signal handling and cleanup.

## Timeout

The executor should enforce the assignment timeout because it directly owns the harness process tree.

This permits the executor to:

- stop the harness cleanly;
- collect final output;
- emit an appropriate terminal result.

The daemon should also maintain an outer deadline with additional grace as a safety backstop in case the executor itself hangs.

The inner timeout is authoritative for normal timeout reporting.

The outer timeout exists to protect daemon capacity from a broken executor.

## Graceful daemon shutdown

On daemon shutdown:

- stop accepting new assignments;
- mark the runner draining;
- continue required Tines reconciliation;
- terminate active executors in a controlled way;
- settle runs as interrupted where the current protocol permits;
- avoid leaving executor/container processes behind.

The executor does not independently poll Tines and therefore does not need daemon-shutdown semantics beyond responding correctly to termination.

## Crash recovery

The daemon persists enough information about active executor processes to reconcile after an ungraceful crash.

The persisted identity should refer to the executor transport process/process group rather than directly to Codex.

For example:

```text
daemon
  ↓
docker run ...
  ↓
tines-runner-rs execute
  ↓
codex
```

The daemon supervises the `docker run ...` execution boundary.

The executor is responsible for supervising Codex inside that boundary.

Containerized executor configurations should be constructed so termination or abandonment of the outer transport does not intentionally leave unmanaged long-lived containers.

Container-engine-specific lifecycle recovery is outside the first version of this proposal.

## Run key and security boundary

The long-lived Tines runner token remains in the daemon only.

The executor receives the per-run `run_key`.

This provides a useful credential boundary:

```text
daemon:
  runner token
  run key

executor:
  run key only

harness:
  run key only
```

The executor can expose the run key to the harness as:

```text
TINES_API_KEY
```

and the server URL as:

```text
TINES_API_URL
```

as today.

The executor may also use the run key itself for issue-scoped Tines API operations if a future execution feature genuinely requires them.

Neither the run key nor assignment environment secrets are placed in argv.

## Capability discovery

Moving Codex into the executor environment means the daemon can no longer assume that inspecting the host's `codex` binary describes the harness that will actually execute the run.

Capability discovery must therefore cross the same executor boundary.

The executor should expose a non-run mode such as:

```text
tines-runner-rs capabilities
```

The daemon can invoke:

```text
<executor argv...> capabilities
```

and receive a versioned JSON capability document.

For example:

```json
{
  "version": 1,
  "harnesses": {
    "codex": {
      "version": "codex-cli 0.153.4",
      "effort": {
        "models": {}
      }
    }
  }
}
```

The exact effort structure should reuse the runner's existing capability model where practical.

This makes capability advertisement describe the actual container/executor environment rather than the daemon host.

Capability probing should be cached and refreshed according to the existing effort-capability policy rather than being executed on every poll.

If the configured executor cannot demonstrate support for the configured harness or required effort, the daemon must fail closed or decline incompatible work instead of silently degrading.

## Future harnesses

The daemon-to-executor protocol should be harness-neutral from its first version.

For example:

```json
{
  "execution": {
    "harness": "codex"
  }
}
```

may later become:

```json
{
  "execution": {
    "harness": "claude_code"
  }
}
```

without changing:

- Tines polling;
- executor transport;
- stdin assignment delivery;
- generic JSONL log handling;
- Tines log sequencing;
- cancellation ownership;
- Tines finish reporting.

Only the executor-side adapter changes.

Each adapter is responsible for translating its native behavior into the common execution-event model.

The exact executable command for each harness remains an executor-environment concern rather than part of the Tines assignment.

## Current assignment versus local execution request

The first implementation should continue using the existing Tines assignment as the source of execution context.

The daemon wraps that assignment in the local `ExecutionRequest` and adds locally resolved execution policy such as:

- harness;
- workspace policy;
- executor-protocol version.

This requires no Tines server change.

A future Tines API could expose an immutable run execution-context document retrievable by the run key. If such an endpoint is added, the executor contract could later accept a thinner bootstrap request.

That is not required for this split.

Passing the current assignment snapshot has an important property: prompt, skills, repositories, environment and scheduling choices remain the same values that Tines supplied when the run was assigned rather than being reconstructed from potentially changed live context.

## Configuration migration

Conceptually:

```toml
wrapper = [...]
```

becomes:

```toml
executor = [...]
```

The important semantic change is:

```text
before:
wrapper → codex

after:
executor → tines-runner-rs execute → codex
```

`executor` should remain:

- an argv array;
- directly spawned;
- shell-free;
- eligible for project/workflow/state overrides.

`runner_type` may remain as the configuration name initially, although `harness` would be a clearer long-term term.

The daemon resolves the effective harness and executor before launching a run and places the harness identity into the execution request.

If backward compatibility is desirable, `wrapper` may temporarily be accepted as a deprecated alias or migrated with a clear configuration error and documentation.

The implementation should avoid maintaining two subtly different long-term execution models.

## Native execution

Container use must not become mandatory.

The default executor should permit the same binary to execute assignments locally.

Conceptually:

```text
tines-runner-rs daemon
        ↓
tines-runner-rs execute
        ↓
codex
```

This should provide behavior equivalent to the current non-wrapper execution path.

The split is therefore an architectural boundary, not a requirement for an additional deployment component.

## Testing

### Protocol unit tests

Cover:

- execution-request serialization;
- execution-request version rejection;
- all execution event variants;
- unknown additive event handling;
- malformed JSONL;
- oversized event lines;
- EOF without a result;
- duplicate terminal results;
- output after terminal result;
- secret redaction.

### Executor tests

Exercise `tines-runner-rs execute` directly using assignment fixtures.

Verify:

- workspace materialization;
- skill materialization;
- repository cloning;
- assignment environment delivery;
- run-key delivery;
- Codex argv construction;
- Codex JSONL parsing;
- usage extraction;
- session extraction;
- timeout;
- rate limiting;
- workspace retention;
- terminal result emission.

A useful developer workflow should be:

```sh
cat assignment.json \
  | tines-runner-rs execute \
  | jq .
```

No live Tines runner registration should be required for executor-level tests that do not require issue-scoped API access.

### Daemon tests

Use a stub executor that speaks the generic JSONL protocol.

Verify:

- assignment-to-execution-request translation;
- executor command selection;
- executor stdin delivery;
- log-event translation;
- Tines log sequencing;
- terminal result translation;
- executor crash;
- malformed executor output;
- cancellation;
- timeout backstop;
- concurrency;
- daemon shutdown;
- crash recovery.

The daemon tests should no longer need to simulate Codex JSONL directly.

### Container acceptance test

Provide at least one repeatable test where:

```text
daemon on host
   ↓
docker/podman executor
   ↓
stub harness
```

verifies:

- assignment JSON crosses stdin;
- workspace creation occurs inside the executor environment;
- repositories are materialized there;
- executor JSONL returns across stdout;
- cancellation terminates the containerized execution;
- the daemon reports logs and completion correctly.

## Implementation approach

A reasonable migration sequence is:

1. Define `ExecutionRequest` and `ExecutionEvent` version 1.
2. Add the `execute` subcommand.
3. Move workspace materialization and repository cloning behind the executor entry point.
4. Move Codex invocation and Codex stream parsing behind the executor entry point.
5. Make executor stdout emit only the generic JSONL protocol.
6. Refactor the daemon to launch the local executor and consume generic events.
7. Move timeout enforcement into the executor with an outer daemon backstop.
8. Add executor capability discovery.
9. Change the current wrapper boundary into the configurable executor boundary.
10. Add container integration coverage.
11. Update README and operating documentation.
12. Remove obsolete code paths that allow the daemon to invoke Codex directly.

The implementation may temporarily support old and new paths during development, but the finished architecture should have one execution path.

## Resulting architecture

After the change, the primary dependency directions are:

```text
                   ┌────────────────────┐
                   │       Tines        │
                   └─────────┬──────────┘
                             │
                    runner protocol
                             │
                   ┌─────────▼──────────┐
                   │    runner daemon   │
                   │                    │
                   │ registration       │
                   │ polling            │
                   │ overrides          │
                   │ concurrency        │
                   │ cancellation       │
                   │ logs / finish      │
                   └─────────┬──────────┘
                             │
                    execution request
                       JSON via stdin
                             │
                   ┌─────────▼──────────┐
                   │ executor transport │
                   │                    │
                   │ native / Docker /  │
                   │ Podman / sandbox   │
                   └─────────┬──────────┘
                             │
                   ┌─────────▼──────────┐
                   │      executor      │
                   │                    │
                   │ workspace          │
                   │ repos / skills     │
                   │ harness adapter    │
                   │ process lifecycle  │
                   └─────────┬──────────┘
                             │
                         harness
                             │
                   ┌─────────▼──────────┐
                   │       Codex        │
                   └────────────────────┘

executor → daemon:
versioned generic JSONL
```

## Success criteria

The split is complete when:

- the daemon no longer directly invokes Codex;
- the daemon can run without Codex installed on the host when the executor environment provides it;
- one assignment is delivered to the executor as JSON through stdin;
- the executor can materialize the complete cold-run workspace from that request;
- the executor can clone repositories and run Codex;
- the executor emits only versioned generic JSONL on stdout;
- the daemon translates executor events into Tines run logs and finish reporting;
- the run key and secret assignment values never appear in executor argv;
- Tines supervisor cancellation terminates the executor without duplicate finish reporting;
- effort capabilities reflect the actual executor environment;
- the default native executor remains functional;
- an operator can replace native execution with a Docker-based executor using configuration only;
- project/workflow/state overrides can select different executor configurations;
- the protocol has a clear path to additional harness adapters without changing daemon/Tines orchestration.