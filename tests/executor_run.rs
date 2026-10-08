#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tines_runner_rs::execution_protocol::ExecutionEventParser;
use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("executor-run-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ExecutorOutput {
    success: bool,
    events: Vec<Value>,
    stdout: String,
    stderr: String,
}

fn request(workspace_parent: &Path, retention: &str, timeout_minutes: u64) -> Value {
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json"))
            .expect("valid execution fixture");
    request["execution"]["workspace"]["parent"] =
        Value::String(workspace_parent.to_string_lossy().into_owned());
    request["execution"]["retention"]["mode"] = Value::String(retention.to_owned());
    request["assignment"]["effort"] = Value::Null;
    request["assignment"]["timeout_minutes"] = json!(timeout_minutes);
    request
}

fn run_executor(directory: &Path, request: &Value, codex: &str) -> ExecutorOutput {
    run_executor_with_env(directory, request, codex, &[])
}

fn run_executor_with_env(
    directory: &Path,
    request: &Value,
    codex: &str,
    inherited_env: &[(&str, &str)],
) -> ExecutorOutput {
    let bin_directory = directory.join("bin");
    fs::create_dir_all(&bin_directory).expect("create stub bin directory");
    let stub = bin_directory.join("codex");
    fs::write(&stub, codex).expect("write Codex stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("make stub executable");

    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_directory.clone()).chain(std::env::split_paths(&current_path)),
    )
    .expect("compose executor PATH");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    command
        .arg("execute")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in inherited_env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("start executor");
    serde_json::to_writer(
        child.stdin.take().expect("executor stdin is piped"),
        request,
    )
    .expect("write executor request");
    let output = child.wait_with_output().expect("wait for executor");

    let mut parser = ExecutionEventParser::default();
    let mut events = parser
        .push(&output.stdout)
        .expect("executor output is valid JSONL");
    if let Some(terminal) = parser.finish().expect("executor emits one result") {
        events.push(terminal);
    }
    ExecutorOutput {
        success: output.status.success(),
        events: events
            .into_iter()
            .map(|event| serde_json::to_value(event).expect("event is serializable"))
            .collect(),
        stdout: String::from_utf8(output.stdout).expect("executor output is valid UTF-8"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run_antigravity_executor(directory: &Path, request: &Value, agy: &str) -> ExecutorOutput {
    let bin_directory = directory.join("antigravity-bin");
    fs::create_dir_all(&bin_directory).expect("create Antigravity stub directory");
    let stub = bin_directory.join("agy");
    fs::write(&stub, agy).expect("write Antigravity stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))
        .expect("make Antigravity stub executable");
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_directory).chain(std::env::split_paths(&current_path)),
    )
    .expect("compose executor PATH");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    command
        .arg("execute")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start executor");
    serde_json::to_writer(
        child.stdin.take().expect("executor stdin is piped"),
        request,
    )
    .expect("write executor request");
    let output = child.wait_with_output().expect("wait for executor");
    let mut parser = ExecutionEventParser::default();
    let mut events = parser
        .push(&output.stdout)
        .expect("executor output is valid JSONL");
    if let Some(terminal) = parser.finish().unwrap_or_else(|error| {
        panic!(
            "Antigravity executor output has a protocol error: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }) {
        events.push(terminal);
    }
    ExecutorOutput {
        success: output.status.success(),
        events: events
            .into_iter()
            .map(|event| serde_json::to_value(event).expect("event is serializable"))
            .collect(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn antigravity_stream_script(records: &[Value]) -> String {
    let jsonl = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize Antigravity fixture"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("#!/bin/sh\ncat >/dev/null\ncat <<'TINES_AGY_STREAM'\n{jsonl}\nTINES_AGY_STREAM\n")
}

fn result(output: &ExecutorOutput) -> &Value {
    let results = output
        .events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "one terminal result: {:?}", output.events);
    results[0]
}

fn simple_success() -> &'static str {
    r##"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"thread-success"}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":42,"cached_input_tokens":10,"cache_write_input_tokens":2,"output_tokens":7}}'
"##
}

fn custom_request(workspace_parent: &Path, command: Vec<String>) -> Value {
    let mut request = request(workspace_parent, "failed", 120);
    request["execution"]["harness"] = Value::String("custom".to_owned());
    request["execution"]["custom_command"] = json!(command);
    request
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write custom harness script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("make custom harness executable");
}

#[test]
fn successful_execution_emits_one_result_and_applies_always_retention() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let output = run_executor(
        &directory.0,
        &request(&workspace_parent, "always", 120),
        simple_success(),
    );

    assert!(output.success, "{}", output.stderr);
    let terminal = result(&output);
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["provider_session_id"], "thread-success");
    assert_eq!(terminal["usage"]["input_tokens"], 30);
    assert_eq!(terminal["usage"]["cache_read_tokens"], 10);
    assert_eq!(terminal["usage"]["cache_write_tokens"], 2);
    assert_eq!(terminal["usage"]["output_tokens"], 7);
    assert_eq!(terminal["pricing_evidence"]["provider"], "codex");
    assert_eq!(terminal["pricing_evidence"]["version"], 1);
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["measurement_status"],
        "complete"
    );
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["raw_usage"]["input_tokens"],
        42
    );
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["raw_usage"]["cached_input_tokens"],
        10
    );
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["raw_usage"]["cache_write_input_tokens"],
        2
    );

    let workspaces = fs::read_dir(workspace_parent)
        .expect("workspace parent remains")
        .map(|entry| entry.expect("workspace entry").path())
        .collect::<Vec<_>>();
    assert_eq!(workspaces.len(), 1);
    let marker: Value = serde_json::from_slice(
        &fs::read(workspaces[0].join(".tines-runner-retained.json")).expect("retention marker"),
    )
    .expect("valid retention marker");
    assert_eq!(marker["terminal_status"], "completed");
    assert_eq!(marker["issue_ref"], "Tines/4");
}

#[test]
fn custom_github_checks_style_command_runs_with_placeholders_environment_and_generic_logs() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("custom-workspaces");
    let script_path = directory.0.join("github-checks-style");
    let interpolation_target = directory.0.join("should-not-exist");
    let injected_argument = format!("literal; $(touch {})", interpolation_target.display());
    let script = format!(
        "#!/bin/sh\n\
         [ \"$1\" = '{injected_argument}' ] || exit 31\n\
         [ -f \"$2\" ] || exit 32\n\
         [ \"$(cat \"$2\")\" = 'Implement the assigned issue.' ] || exit 33\n\
         [ \"$3\" = \"$PWD\" ] || exit 34\n\
         [ \"$4\" = \"prefix=$PWD\" ] || exit 35\n\
         [ \"$TINES_API_KEY\" = 'fixture-run-key' ] || exit 36\n\
         [ \"$TINES_API_URL\" = 'https://tines.example.test' ] || exit 37\n\
         [ \"$FIXTURE_TOKEN\" = 'fixture-secret' ] || exit 38\n\
         [ \"${{TINES_RUNNER_TOKEN+x}}\" != x ] || exit 39\n\
         [ \"${{TYPESAFE_API_KEY+x}}\" != x ] || exit 40\n\
         printf '%s\\n' 'github-checks style fixture passed'\n\
         printf '%s\\n' 'custom stderr diagnostic' >&2\n"
    );
    write_executable(&script_path, &script);
    let command = vec![
        script_path.to_string_lossy().into_owned(),
        injected_argument,
        "{prompt_file}".to_owned(),
        "{workspace}".to_owned(),
        "prefix={workspace}".to_owned(),
    ];
    let request = custom_request(&workspace_parent, command);

    let output = run_executor_with_env(
        &directory.0,
        &request,
        "#!/bin/sh\nexit 99\n",
        &[
            ("TINES_API_KEY", "long-lived-bootstrap-key"),
            ("TINES_API_URL", "https://daemon.example.test"),
            ("TINES_RUNNER_TOKEN", "long-lived-runner-token"),
            ("TYPESAFE_API_KEY", "typesafe-key"),
        ],
    );

    assert!(
        output.success,
        "stderr: {}\nevents: {:#?}",
        output.stderr, output.events
    );
    assert_eq!(result(&output)["status"], "completed");
    assert!(
        !interpolation_target.exists(),
        "argv was shell-interpolated"
    );
    let logs = output
        .events
        .iter()
        .filter(|event| event["type"] == "log")
        .collect::<Vec<_>>();
    assert!(logs.iter().any(|event| {
        event["stream"] == "stdout"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("github-checks style fixture passed"))
    }));
    let stderr = logs
        .iter()
        .filter(|event| event["stream"] == "stderr")
        .filter_map(|event| event["message"].as_str())
        .collect::<String>();
    assert!(stderr.contains("custom stderr diagnostic"));
    let rendered = serde_json::to_string(&output.events).unwrap();
    for secret in [
        "fixture-secret",
        "fixture-run-key",
        "long-lived-bootstrap-key",
        "long-lived-runner-token",
        "typesafe-key",
    ] {
        assert!(!rendered.contains(secret), "secret {secret} reached logs");
    }
}

#[test]
fn antigravity_launches_with_exact_model_stream_prompt_and_eof() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("antigravity-workspaces");
    let control = directory.0.join("agy-control");
    fs::create_dir_all(&control).expect("create Antigravity control directory");
    let mut request = request(&workspace_parent, "never", 1);
    request["execution"]["harness"] = Value::String("antigravity".to_owned());
    request["assignment"]["effort"] = Value::Null;
    request["assignment"]["run"]["model"] = Value::String("gemini-custom.v2".to_owned());
    request["assignment"]["prompt"] = Value::String("Use stdin exactly.".to_owned());
    request["assignment"]["env"] = serde_json::json!([
        { "name": "AGY_CONTROL", "value": control, "secret": false }
    ]);
    let script = r##"#!/bin/sh
printf '%s\n' "$@" > "$AGY_CONTROL/args"
pwd > "$AGY_CONTROL/cwd"
cat > "$AGY_CONTROL/input"
printf '%s\n' '{"event":"init","conversation_id":"agy-session-42","init":{}}' '{"event":"step_update","step_update":{"conversation_id":"agy-session-42","step_type":"agent_response","text_delta":"Antigravity answer"}}' '{"event":"result","result":{"conversation_id":"agy-session-42","status":"SUCCESS","response":"Antigravity answer","usage":{"input_tokens":12,"output_tokens":4,"cache_read_tokens":3,"thinking_tokens":99}}}'
"##;

    let output = run_antigravity_executor(&directory.0, &request, script);

    assert!(
        output.success,
        "stderr: {}\nevents: {:#?}",
        output.stderr, output.events
    );
    assert_eq!(result(&output)["status"], "completed");
    assert_eq!(result(&output)["provider_session_id"], "agy-session-42");
    assert_eq!(result(&output)["usage"]["input_tokens"], 12);
    assert_eq!(result(&output)["usage"]["output_tokens"], 4);
    assert_eq!(result(&output)["usage"]["cache_read_tokens"], 3);
    let args = fs::read_to_string(control.join("args")).expect("captured agy arguments");
    let args = args.lines().collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--sandbox",
            "--print-timeout",
            "2m",
            "--model",
            "gemini-custom.v2",
        ]
    );
    let prompt: Value = serde_json::from_slice(
        &fs::read(control.join("input")).expect("captured agy stream input"),
    )
    .expect("valid stream input JSON");
    assert_eq!(prompt["event"], "user");
    assert_eq!(prompt["message"]["content"], "Use stdin exactly.");
    assert!(
        fs::read_to_string(control.join("cwd"))
            .expect("captured agy working directory")
            .trim()
            .contains("antigravity-workspaces"),
        "agy must run in the materialized workspace"
    );
    assert_eq!(
        output
            .events
            .iter()
            .filter(|event| event["type"] == "log" && event["stream"] == "stdout")
            .filter_map(|event| event["message"].as_str())
            .filter(|message| *message == "Antigravity answer")
            .count(),
        1,
        "streamed response must not repeat from result.response"
    );
}

#[test]
fn antigravity_redacts_split_responses_and_clipped_fields_through_retention() {
    let directory = TestDirectory::new();
    let secret = "SYNTHETIC_REVIEW_SECRET_VALUE";

    let streamed_workspace = directory.0.join("streamed-workspaces");
    let mut streamed_request = request(&streamed_workspace, "always", 1);
    streamed_request["execution"]["harness"] = Value::String("antigravity".to_owned());
    streamed_request["assignment"]["env"] = json!([
        { "name": "SYNTHETIC_SECRET", "value": secret, "secret": true }
    ]);
    let streamed_output = run_antigravity_executor(
        &directory.0,
        &streamed_request,
        &antigravity_stream_script(&[
            json!({
                "event": "step_update",
                "step_update": { "step_type": "agent_response", "text_delta": "before SYNTHETIC_REVIEW_" }
            }),
            json!({
                "event": "step_update",
                "step_update": { "step_type": "agent_response", "text_delta": "SECRET_VALUE after" }
            }),
            json!({ "event": "result", "result": { "status": "SUCCESS" } }),
        ]),
    );
    assert!(streamed_output.success, "{}", streamed_output.stderr);
    assert!(!streamed_output.stdout.contains("SYNTHETIC_REVIEW_SECRET"));
    let streamed_logs = streamed_output
        .events
        .iter()
        .filter_map(|event| {
            (event["type"] == "log" && event["stream"] == "stdout")
                .then(|| event["message"].as_str())
                .flatten()
        })
        .collect::<String>();
    assert!(streamed_logs.contains("***"));
    assert!(!streamed_logs.contains(secret));

    let fallback_workspace = directory.0.join("fallback-workspaces");
    let mut fallback_request = request(&fallback_workspace, "always", 1);
    fallback_request["execution"]["harness"] = Value::String("antigravity".to_owned());
    fallback_request["assignment"]["env"] = json!([
        { "name": "SYNTHETIC_SECRET", "value": secret, "secret": true }
    ]);
    let prefix = "x".repeat(32 * 1024 - 8);
    let fallback_output = run_antigravity_executor(
        &directory.0,
        &fallback_request,
        &antigravity_stream_script(&[json!({
            "event": "result",
            "result": { "status": "SUCCESS", "response": format!("{prefix}{secret}tail") }
        })]),
    );
    assert!(fallback_output.success, "{}", fallback_output.stderr);
    assert!(!fallback_output.stdout.contains(&secret[..8]));
    let fallback_logs = fallback_output
        .events
        .iter()
        .filter_map(|event| {
            (event["type"] == "log" && event["stream"] == "stdout")
                .then(|| event["message"].as_str())
                .flatten()
        })
        .collect::<String>();
    assert!(fallback_logs.contains("***"));
    assert!(!fallback_logs.contains(secret));
    assert!(!fallback_logs.contains(&secret[..8]));

    let failed_workspace = directory.0.join("failed-workspaces");
    let tool_secret = "TOOL_REDACTION_SECRET_VALUE";
    let mut failed_request = request(&failed_workspace, "failed", 1);
    failed_request["execution"]["harness"] = Value::String("antigravity".to_owned());
    failed_request["assignment"]["env"] = json!([
        { "name": "SYNTHETIC_SECRET", "value": secret, "secret": true },
        { "name": "TOOL_SECRET", "value": tool_secret, "secret": true }
    ]);
    let failed_output = run_antigravity_executor(
        &directory.0,
        &failed_request,
        &antigravity_stream_script(&[
            json!({
                "event": "step_update",
                "step_update": {
                    "step_type": "tool",
                    "tool_name": format!("{}{}", "x".repeat(152), tool_secret),
                    "state": format!("{}{}", "y".repeat(24), tool_secret)
                }
            }),
            json!({
                "event": "result",
                "result": {
                    "status": "ERROR",
                    "error": format!("{}{}", "f".repeat(1_990), secret)
                }
            }),
        ]),
    );
    assert!(!failed_output.success);
    assert!(!failed_output.stdout.contains(&secret[..10]));
    assert!(!failed_output.stdout.contains(&tool_secret[..8]));
    let rendered_events = serde_json::to_string(&failed_output.events).expect("events serialize");
    assert!(!rendered_events.contains(&secret[..10]));
    assert!(!rendered_events.contains(&tool_secret[..8]));
    assert!(
        !result(&failed_output)["error"]
            .as_str()
            .expect("terminal diagnostic")
            .contains(&secret[..10])
    );
    let retained_workspace = fs::read_dir(&failed_workspace)
        .expect("failed workspace parent remains")
        .next()
        .expect("failed workspace retained")
        .expect("workspace entry")
        .path();
    let marker: Value = serde_json::from_slice(
        &fs::read(retained_workspace.join(".tines-runner-retained.json"))
            .expect("retention marker"),
    )
    .expect("valid retention marker");
    let retained_error = marker["error"].as_str().expect("retained error");
    assert!(retained_error.contains("***"));
    assert!(!retained_error.contains(&secret[..10]));
}

#[test]
fn antigravity_effort_is_rejected_before_the_harness_starts() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("antigravity-effort-workspaces");
    let marker = directory.0.join("agy-started");
    let mut request = request(&workspace_parent, "never", 1);
    request["execution"]["harness"] = Value::String("antigravity".to_owned());
    request["assignment"]["effort"] = serde_json::json!({
        "version": 1,
        "value": "high",
        "capability_digest": "catalog",
    });
    let script = format!("#!/bin/sh\nprintf started > {:?}\n", marker);

    let output = run_antigravity_executor(&directory.0, &request, &script);

    assert!(!output.success);
    assert!(
        !marker.exists(),
        "explicit effort must be rejected before agy starts"
    );
    assert!(
        result(&output)["error"]
            .as_str()
            .expect("deterministic rejection")
            .contains("Antigravity explicit effort delivery is not supported")
    );
}

#[test]
fn custom_nonzero_exit_fails_with_redacted_stderr_context() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("custom-workspaces");
    let script_path = directory.0.join("failing-check");
    let before_boundary = "x".repeat(8 * 1024 - 4);
    let after_secret = "y".repeat(
        16 * 1024 - before_boundary.len() - "fixture-secret".len() - "fixture-run-key".len(),
    );
    let script = format!(
        "#!/bin/sh\nprintf '%s' '{before_boundary}' >&2\nprintf '%s' 'fixture-secret' >&2\nprintf '%s' '{after_secret}' >&2\nprintf '%s' 'fixture-run-key' >&2\nexit 7\n"
    );
    write_executable(&script_path, &script);
    let request = custom_request(
        &workspace_parent,
        vec![script_path.to_string_lossy().into_owned()],
    );

    let output = run_executor(&directory.0, &request, "#!/bin/sh\nexit 99\n");

    assert!(!output.success);
    let terminal = result(&output);
    assert_eq!(terminal["status"], "failed");
    assert_eq!(terminal["exit_code"], 7);
    let error = terminal["error"].as_str().expect("failure diagnostic");
    assert!(error.contains("custom harness exited with code 7"));
    assert!(error.contains("custom harness stderr"));
    assert!(error.contains("***"));
    assert!(!error.contains("fixture-secret"));
    assert!(!error.contains("e-secret"));
    assert!(output.events.iter().any(|event| {
        event["type"] == "log"
            && event["stream"] == "stderr"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("***"))
    }));
    let rendered_events = serde_json::to_string(&output.events).expect("serialize test events");
    for secret in ["fixture-secret", "e-secret", "fixture-run-key"] {
        assert!(
            !rendered_events.contains(secret),
            "secret fragment {secret} reached executor events"
        );
    }

    let workspace = fs::read_dir(workspace_parent)
        .expect("failed workspace parent remains")
        .next()
        .expect("failed workspace retained")
        .expect("workspace entry")
        .path();
    let marker: Value = serde_json::from_slice(
        &fs::read(workspace.join(".tines-runner-retained.json")).expect("retention marker"),
    )
    .expect("valid retention marker");
    let retained_error = marker["error"].as_str().expect("retained error diagnostic");
    assert!(retained_error.contains("***"));
    for secret in ["fixture-secret", "e-secret", "fixture-run-key"] {
        assert!(
            !retained_error.contains(secret),
            "secret fragment {secret} reached retained metadata"
        );
    }
}

#[test]
fn environment_run_key_is_redacted_from_split_output_and_retained_metadata() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("environment-key-workspaces");
    let run_key = "environment-\"quoted\"-\\key";
    let (first, second) = run_key.split_at("environment-".len());
    let json_escaped = serde_json::to_string(run_key).expect("encode fixture run key");
    let json_escaped = &json_escaped[1..json_escaped.len() - 1];
    let script_path = directory.0.join("environment-key-check");
    let script = format!(
        "#!/bin/sh\n\
         printf '%s' '{first}'\n\
         sleep 0.05\n\
         printf '%s\\n' '{second}'\n\
         printf '%s\\n' '{json_escaped}'\n\
         printf '%s' '{first}' >&2\n\
         sleep 0.05\n\
         printf '%s\\n' '{second}' >&2\n\
         printf '%s\\n' '{json_escaped}' >&2\n\
         exit 1\n"
    );
    write_executable(&script_path, &script);
    let mut request = custom_request(
        &workspace_parent,
        vec![script_path.to_string_lossy().into_owned()],
    );
    request["assignment"]["run_key"] = Value::Null;

    let output = run_executor_with_env(
        &directory.0,
        &request,
        "#!/bin/sh\nexit 99\n",
        &[("TINES_API_KEY", run_key)],
    );

    assert!(!output.success);
    for secret_form in [run_key, json_escaped] {
        assert!(
            !output.stdout.contains(secret_form),
            "secret form {secret_form:?} reached raw executor JSONL: {}",
            output.stdout
        );
        assert!(
            !output.stderr.contains(secret_form),
            "secret form {secret_form:?} reached executor diagnostics"
        );
    }
    let rendered_events = serde_json::to_string(&output.events).expect("serialize events");
    for secret_form in [run_key, json_escaped] {
        assert!(!rendered_events.contains(secret_form));
    }
    for stream in ["stdout", "stderr"] {
        assert!(
            output.events.iter().any(|event| {
                event["type"] == "log"
                    && event["stream"] == stream
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("***"))
            }),
            "expected a redacted {stream} log: {:?}",
            output.events
        );
    }

    let terminal = result(&output);
    let error = terminal["error"]
        .as_str()
        .expect("terminal failure diagnostic");
    assert!(error.contains("***"));
    for secret_form in [run_key, json_escaped] {
        assert!(!error.contains(secret_form));
    }

    let workspace = fs::read_dir(workspace_parent)
        .expect("workspace parent remains")
        .next()
        .expect("failed workspace retained")
        .expect("workspace entry")
        .path();
    let marker: Value = serde_json::from_slice(
        &fs::read(workspace.join(".tines-runner-retained.json")).expect("retention marker"),
    )
    .expect("valid retention marker");
    let retained_error = marker["error"].as_str().expect("retained error diagnostic");
    assert!(retained_error.contains("***"));
    for secret_form in [run_key, json_escaped] {
        assert!(!retained_error.contains(secret_form));
    }
}

#[test]
fn nonzero_harness_exit_keeps_usage_and_session_in_failed_result() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let output = run_executor(
        &directory.0,
        &request(&workspace_parent, "failed", 120),
        &format!("{}exit 9\n", simple_success()),
    );

    assert!(!output.success);
    let terminal = result(&output);
    assert_eq!(terminal["status"], "failed");
    assert_eq!(terminal["exit_code"], 9);
    assert_eq!(terminal["provider_session_id"], "thread-success");
    assert_eq!(terminal["usage"]["input_tokens"], 30);
    assert_eq!(terminal["pricing_evidence"]["provider"], "codex");
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["raw_usage"]["input_tokens"],
        42
    );

    let workspace = fs::read_dir(workspace_parent)
        .expect("workspace parent remains")
        .next()
        .expect("failed workspace retained")
        .expect("workspace entry")
        .path();
    let marker: Value = serde_json::from_slice(
        &fs::read(workspace.join(".tines-runner-retained.json")).expect("retention marker"),
    )
    .expect("valid retention marker");
    assert_eq!(marker["terminal_status"], "failed");
}

#[test]
fn rate_limit_after_usage_emits_self_contained_rate_limited_result() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let script = r##"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"thread-limited"}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":120,"cached_input_tokens":20,"cache_write_input_tokens":5,"output_tokens":8}}'
printf '%s\n' '{"type":"turn.failed","error":{"message":"You have hit your usage limit. Try again later.","codex_error_info":"usage_limit_exceeded","resets_at":1791000000}}'
exit 1
"##;
    let output = run_executor(
        &directory.0,
        &request(&workspace_parent, "failed", 120),
        script,
    );

    assert!(!output.success);
    let terminal = result(&output);
    assert_eq!(terminal["status"], "rate_limited");
    assert_eq!(terminal["provider_session_id"], "thread-limited");
    assert_eq!(terminal["usage"]["input_tokens"], 95);
    assert_eq!(terminal["usage"]["cache_read_tokens"], 20);
    assert!(terminal["rate_limit"]["resume_at"].as_u64().is_some());
    assert_eq!(
        terminal["pricing_evidence"]["payload"]["raw_usage"]["input_tokens"],
        120
    );
}

#[test]
fn never_retention_removes_successful_workspace() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let output = run_executor(
        &directory.0,
        &request(&workspace_parent, "never", 120),
        simple_success(),
    );

    assert!(output.success, "{}", output.stderr);
    assert_eq!(result(&output)["status"], "completed");
    assert!(
        fs::read_dir(workspace_parent)
            .expect("workspace parent remains")
            .next()
            .is_none()
    );
}

#[test]
fn timeout_terminates_harness_descendants_and_preserves_observed_usage() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let pid_file = directory.0.join("descendant.pid");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"thread-timeout\"}}'\nprintf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":70,\"cached_input_tokens\":10,\"cache_write_input_tokens\":5,\"output_tokens\":13}}}}'\ntrap '' TERM\nsleep 600 &\nchild=$!\nprintf '%s\\n' \"$child\" > '{}.tmp'\nmv '{}.tmp' '{}'\nwhile :; do sleep 5; done\n",
        pid_file.display(),
        pid_file.display(),
        pid_file.display(),
    );
    let output = run_executor(
        &directory.0,
        &request(&workspace_parent, "failed", 1),
        &script,
    );

    assert!(!output.success);
    let terminal = result(&output);
    assert_eq!(terminal["status"], "failed");
    assert!(terminal["error"].as_str().unwrap().contains("timeout"));
    assert_eq!(terminal["provider_session_id"], "thread-timeout");
    assert_eq!(terminal["usage"]["input_tokens"], 55);

    let pid: u32 = fs::read_to_string(pid_file)
        .expect("child process published its PID")
        .trim()
        .parse()
        .expect("valid PID");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !process_running(pid),
        "harness descendant {pid} is still running"
    );
}

fn process_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return false;
        };
        fields
            .split_whitespace()
            .next()
            .is_some_and(|state| state != "Z")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|state| state.trim().chars().next())
            .is_some_and(|state| state != 'Z')
    }
}
