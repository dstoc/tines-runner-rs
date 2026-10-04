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
    let mut child = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
        .arg("execute")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start executor");
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
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
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
