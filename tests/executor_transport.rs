#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tines_runner_rs::config::{Config, MatchContext};
use tines_runner_rs::execution_protocol::{
    EXECUTION_PROTOCOL_VERSION, ExecutionRequest, ExecutionRetentionPolicy, LocalExecutionPolicy,
    TinesExecutionContext, WorkspacePolicy,
};
use tines_runner_rs::executor_transport::{ExecutorTransport, ExecutorTransportError};
use tines_runner_rs::process::ProcessExit;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tines-runner-executor-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executor stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("make executor stub executable");
}

fn configured_transport(argv: Vec<String>, cwd: &Path) -> ExecutorTransport {
    let argv = serde_json::to_string(&argv).expect("serialize executor argv");
    let cwd =
        serde_json::to_string(&cwd.to_string_lossy().as_ref()).expect("serialize executor cwd");
    let config = Config::from_toml_str(&format!(
        "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"transport-test\"\nexecutor = {argv}\nexecutor_cwd = {cwd}\n"
    ))
    .expect("parse executor config");
    let resolved = config.resolve(MatchContext {
        project: "Tines",
        workflow: "Implementation",
        state: "Implement",
    });
    ExecutorTransport::from_resolved(&resolved)
}

fn request(prompt: String) -> ExecutionRequest {
    ExecutionRequest {
        version: EXECUTION_PROTOCOL_VERSION,
        tines: TinesExecutionContext {
            api_url: "https://tines.example.test".to_owned(),
        },
        execution: LocalExecutionPolicy {
            harness: "codex".to_owned(),
            workspace: WorkspacePolicy {
                parent: Some(PathBuf::from("/executor/workspaces")),
            },
            retention: ExecutionRetentionPolicy {
                mode: tines_runner_rs::config::RetentionMode::Never,
                max_age_hours: 72,
                max_count: 20,
            },
        },
        assignment: serde_json::from_value(json!({
            "run": {"id": "arun_transport", "issue_id": "iss_transport"},
            "prompt": prompt,
            "bundle": {"skills": [], "repos": []},
            "run_key": "ephemeral-run-key",
            "timeout_minutes": 120,
            "env": [
                {"name": "DEPLOY_TOKEN", "value": "assignment-secret", "secret": true}
            ]
        }))
        .expect("decode assignment fixture"),
    }
}

fn result_event() -> &'static str {
    "{\"version\":1,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}\n"
}

fn codex_capabilities_document() -> &'static str {
    r#"{"version":1,"harnesses":{"codex":{"version":"codex-fake 0.1.0","effort":{"version":1,"daemon_version":"0.1.0","harness":"codex","harness_version":"codex-fake 0.1.0","catalog_digest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","models":[],"accepts_asserted_effort":true}}}}"#
}

#[test]
fn discovers_capabilities_through_the_configured_executor_command() {
    let directory = TestDirectory::new();
    let args_path = directory.0.join("capability-args");
    let stub = directory.0.join("capability-executor");
    write_executable(
        &stub,
        &format!(
            "#!/bin/sh\nif [ -n \"${{TINES_API_KEY:-}}${{TINES_API_URL:-}}${{TINES_RUNNER_TOKEN:-}}${{TYPESAFE_API_KEY:-}}\" ]; then exit 27; fi\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{}'\n",
            args_path.display(),
            codex_capabilities_document()
        ),
    );
    let working_directory = directory.0.join("executor-cwd");
    fs::create_dir_all(&working_directory).expect("create executor cwd");
    let transport = configured_transport(
        vec![
            stub.to_string_lossy().into_owned(),
            "run".to_owned(),
            "--rm".to_owned(),
            "-i".to_owned(),
            "runner-image".to_owned(),
        ],
        &working_directory,
    );

    let capabilities = transport
        .discover_capabilities()
        .expect("read capability document from executor");

    assert!(capabilities.supports("codex"));
    assert_eq!(capabilities.harnesses["codex"].version, "codex-fake 0.1.0");
    assert_eq!(
        fs::read_to_string(args_path).unwrap(),
        "run\n--rm\n-i\nrunner-image\ncapabilities\n"
    );
}

#[test]
fn malformed_executor_capability_response_fails_closed() {
    let directory = TestDirectory::new();
    let stub = directory.0.join("malformed-capability-executor");
    write_executable(&stub, "#!/bin/sh\nprintf '%s\\n' '{not-json}'\n");
    let transport = ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], &directory.0);

    assert!(matches!(
        transport.discover_capabilities(),
        Err(ExecutorTransportError::Capabilities(_))
    ));
}

#[test]
fn valid_document_without_configured_harness_is_unsupported() {
    let directory = TestDirectory::new();
    let stub = directory.0.join("unsupported-capability-executor");
    write_executable(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"version\":1,\"harnesses\":{},\"discovery_error\":\"Codex is not installed\"}'\n",
    );
    let transport = ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], &directory.0);

    let capabilities = transport
        .discover_capabilities()
        .expect("parse valid unsupported capability document");

    assert!(!capabilities.supports("codex"));
    assert!(
        capabilities
            .effort_report("codex", "0.1.0")
            .discovery_error
            .is_some()
    );
}

#[test]
fn sends_large_request_on_stdin_with_explicit_cwd_and_direct_argv() {
    let directory = TestDirectory::new();
    let working_directory = directory.0.join("executor-cwd");
    fs::create_dir_all(&working_directory).expect("create executor cwd");
    let request_path = directory.0.join("request.json");
    let cwd_path = directory.0.join("seen-cwd");
    let args_path = directory.0.join("seen-args");
    let stub = directory.0.join("executor-stub");
    write_executable(
        &stub,
        &format!(
            "#!/bin/sh\ncat > '{}'\npwd > '{}'\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{}'\n",
            request_path.display(),
            cwd_path.display(),
            args_path.display(),
            result_event()
        ),
    );

    let request = request("large assignment ".repeat(100_000));
    let expected_request = serde_json::to_vec(&request).expect("serialize expected request");
    assert!(expected_request.len() > 1_000_000);
    let transport = configured_transport(
        vec![
            stub.to_string_lossy().into_owned(),
            "literal ; argument".to_owned(),
        ],
        &working_directory,
    );
    let mut received_stdout = Vec::new();
    let output = transport
        .run(
            &request,
            Duration::from_secs(15),
            Duration::from_millis(100),
            |chunk| received_stdout.extend_from_slice(chunk),
        )
        .expect("run stub executor");

    assert_eq!(output.exit, ProcessExit::Code(0));
    assert!(!output.timed_out);
    assert_eq!(received_stdout, result_event().as_bytes());
    assert_eq!(fs::read(request_path).unwrap(), expected_request);
    assert_eq!(
        fs::read_to_string(cwd_path).unwrap().trim(),
        working_directory.canonicalize().unwrap().to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(args_path).unwrap(),
        "literal ; argument\nexecute\n"
    );

    let serialized = serde_json::to_value(&request).unwrap();
    assert!(serialized.get("executor_cwd").is_none());
}

#[test]
fn launches_container_style_argv_and_redacts_bounded_stderr() {
    let directory = TestDirectory::new();
    let working_directory = directory.0.join("container-cwd");
    fs::create_dir_all(&working_directory).expect("create executor cwd");
    let args_path = directory.0.join("container-args");
    let cwd_path = directory.0.join("container-cwd-seen");
    let stub = directory.0.join("container-command-stub");
    write_executable(
        &stub,
        &format!(
            "#!/bin/sh\ncat >/dev/null\npwd > '{}'\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{}'\nprintf 'run key=%s token=%s\\n' 'ephemeral-run-key' 'assignment-secret' >&2\nhead -c 40000 /dev/zero | tr '\\000' x >&2\n",
            cwd_path.display(),
            args_path.display(),
            result_event()
        ),
    );
    let request = request("container-style".to_owned());
    let transport = configured_transport(
        vec![
            stub.to_string_lossy().into_owned(),
            "run".to_owned(),
            "--rm".to_owned(),
            "-i".to_owned(),
            "runner-image".to_owned(),
        ],
        &working_directory,
    );
    let output = transport
        .run(
            &request,
            Duration::from_secs(15),
            Duration::from_millis(100),
            |_| {},
        )
        .expect("run container command stub");

    assert_eq!(output.exit, ProcessExit::Code(0));
    assert_eq!(
        fs::read_to_string(args_path).unwrap(),
        "run\n--rm\n-i\nrunner-image\nexecute\n"
    );
    assert_eq!(
        fs::read_to_string(cwd_path).unwrap().trim(),
        working_directory.canonicalize().unwrap().to_string_lossy()
    );
    assert_eq!(
        request.execution.workspace.parent,
        Some(PathBuf::from("/executor/workspaces"))
    );
    assert!(!output.stderr.contains("ephemeral-run-key"));
    assert!(!output.stderr.contains("assignment-secret"));
    assert!(output.stderr.contains("[executor stderr truncated]"));
    assert!(output.stderr.len() <= 32 * 1024);
}

#[test]
fn invalid_executor_cwd_fails_before_running_command() {
    let directory = TestDirectory::new();
    let marker = directory.0.join("started");
    let stub = directory.0.join("executor-stub");
    write_executable(&stub, &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
    let missing_cwd = directory.0.join("missing-cwd");
    let transport = ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], &missing_cwd);
    let error = transport
        .run(
            &request("invalid cwd".to_owned()),
            Duration::from_secs(1),
            Duration::from_millis(100),
            |_| {},
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExecutorTransportError::InvalidWorkingDirectory(_)
    ));
    assert!(!marker.exists());
}

#[test]
fn non_directory_executor_cwd_fails_before_running_command() {
    let directory = TestDirectory::new();
    let marker = directory.0.join("started");
    let stub = directory.0.join("executor-stub");
    write_executable(&stub, &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
    let not_directory = directory.0.join("file-cwd");
    fs::write(&not_directory, "file").expect("write non-directory cwd");
    let transport =
        ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], not_directory);
    let error = transport
        .run(
            &request("invalid cwd".to_owned()),
            Duration::from_secs(1),
            Duration::from_millis(100),
            |_| {},
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExecutorTransportError::InvalidWorkingDirectory(_)
    ));
    assert!(!marker.exists());
}

#[test]
fn outer_deadline_terminates_a_hung_executor() {
    let directory = TestDirectory::new();
    let working_directory = directory.0.join("executor-cwd");
    fs::create_dir_all(&working_directory).expect("create executor cwd");
    let stub = directory.0.join("slow-executor");
    write_executable(&stub, "#!/bin/sh\ncat >/dev/null\nsleep 10\n");
    let transport =
        ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], working_directory);
    let started = std::time::Instant::now();
    let output = transport
        .run(
            &request("timeout".to_owned()),
            Duration::from_millis(100),
            Duration::from_millis(50),
            |_| {},
        )
        .expect("supervise executor timeout");

    assert!(output.timed_out);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn parses_stub_output_as_jsonl_machine_channel() {
    let directory = TestDirectory::new();
    let working_directory = directory.0.join("executor-cwd");
    fs::create_dir_all(&working_directory).expect("create executor cwd");
    let stub = directory.0.join("jsonl-executor");
    write_executable(
        &stub,
        &format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{}'\n",
            result_event()
        ),
    );
    let transport =
        ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], working_directory);
    let mut bytes = Vec::new();
    transport
        .run(
            &request("jsonl".to_owned()),
            Duration::from_secs(2),
            Duration::from_millis(100),
            |chunk| bytes.extend_from_slice(chunk),
        )
        .expect("run JSONL executor");

    let events = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [json!({
            "version": 1,
            "type": "result",
            "status": "completed",
            "exit_code": 0
        })]
    );
}

#[test]
fn review_deadline_with_continuous_output() {
    let directory = TestDirectory::new();
    let stub = directory.0.join("flood-executor");
    write_executable(
        &stub,
        "#!/bin/sh\ncat >/dev/null\nhead -c 4194304 /dev/zero\n",
    );
    let transport = ExecutorTransport::new(vec![stub.to_string_lossy().into_owned()], &directory.0);
    let started = std::time::Instant::now();
    let output = transport
        .run(
            &request("flood".to_owned()),
            Duration::from_millis(100),
            Duration::from_millis(50),
            |_| std::thread::sleep(Duration::from_millis(1)),
        )
        .unwrap();
    assert!(
        output.timed_out,
        "continuous output must not bypass the deadline"
    );
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn review_cwd_without_search_permission() {
    let directory = TestDirectory::new();
    let cwd = directory.0.join("readable-but-not-searchable");
    fs::create_dir(&cwd).unwrap();
    fs::set_permissions(&cwd, fs::Permissions::from_mode(0o400)).unwrap();
    let transport = ExecutorTransport::new(vec!["/bin/true".to_owned()], &cwd);
    let error = transport
        .run(
            &request("cwd".to_owned()),
            Duration::from_secs(1),
            Duration::from_millis(50),
            |_| {},
        )
        .unwrap_err();
    fs::set_permissions(&cwd, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        &error,
        ExecutorTransportError::InvalidWorkingDirectory(_)
    ));
    assert!(error.to_string().contains("executor_cwd"));
}
