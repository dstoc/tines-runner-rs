#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use tines_runner_rs::assignment::{PreparedAssignment, resolve_assignment};
use tines_runner_rs::assignment_worker::{AssignmentTaskOutcome, run_assignment};
use tines_runner_rs::cancellation::CancellationToken;
use tines_runner_rs::config::Config;
use tines_runner_rs::credentials::{CredentialStore, RunnerCredentials};
use tines_runner_rs::effort::EffortCapabilities;
use tines_runner_rs::execution::{
    ExecutionContext, ExecutionOutcome, execute_assignment_cancellable,
};
use tines_runner_rs::executor_capabilities::{ExecutorCapabilities, ExecutorHarnessCapabilities};
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::RunnerAssignmentEffort;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::recovery::ActiveRunStore;
use tines_runner_rs::runner::RunnerConnection;
use tines_runner_rs::shutdown::ShutdownSignal;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tines-runner-cancel-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let count = stream.read(&mut chunk).expect("read fake Tines request");
        assert_ne!(count, 0, "client closed before completing its request");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("request headers");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            request.truncate(header_end + 4 + content_length);
            return String::from_utf8(request).expect("request is UTF-8");
        }
    }
}

fn respond(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write fake Tines response");
}

fn cancellation_server(
    expected_requests: usize,
) -> (String, JoinHandle<Vec<String>>, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
    let address = listener.local_addr().expect("read fake Tines address");
    let (log_seen_tx, log_seen_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().expect("accept fake Tines request");
            let request = read_request(&mut stream);
            if request.starts_with("GET /api/v1/issues/iss_cancel ") {
                respond(
                    &mut stream,
                    r#"{"id":"iss_cancel","workflow":{"name":"Implementation"}}"#,
                );
            } else if request.starts_with("POST /api/v1/runs/arun_cancel/logs ") {
                let (_, body) = request.split_once("\r\n\r\n").expect("log request body");
                let log: serde_json::Value =
                    serde_json::from_str(body).expect("decode log request");
                let seq = log["seq"].as_u64().expect("log sequence");
                respond(
                    &mut stream,
                    &format!(
                        "{{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":{seq}}}"
                    ),
                );
                let _ = log_seen_tx.send(());
            } else if request.starts_with("POST /api/v1/runs/arun_cancel/finish ") {
                respond(&mut stream, r#"{"id":"arun_cancel","status":"failed"}"#);
            } else {
                panic!("unexpected Tines request: {request}");
            }
            requests.push(request);
        }

        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_request(&mut stream);
                    respond(&mut stream, r#"{"error":{"code":"unexpected"}}"#);
                    requests.push(request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept unexpected Tines request: {error}"),
            }
        }
        requests
    });
    (format!("http://{address}"), server, log_seen_rx)
}

fn assignment(pid_file: Option<&PathBuf>) -> RunnerAssignment {
    serde_json::from_value(json!({
        "run": {
            "id": "arun_cancel",
            "issue_id": "iss_cancel",
            "issue_ref": {"project_name": "Tines", "number": 15, "title": "Cancellation"},
            "state_at_start_name": "Implement"
        },
        "prompt": "work",
        "bundle": {"skills": [], "repos": []},
        "run_key": "issue-run-key",
        "timeout_minutes": 5,
        "env": pid_file.map(|path| vec![json!({
            "name": "PID_FILE",
            "value": path.to_string_lossy(),
            "secret": false
        })]).unwrap_or_default()
    }))
    .expect("decode assignment")
}

fn capabilities() -> EffortCapabilities {
    EffortCapabilities {
        version: 1,
        daemon_version: "test-runner".to_owned(),
        harness: "codex".to_owned(),
        harness_version: "stub".to_owned(),
        catalog_digest: "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
            .to_owned(),
        models: Vec::new(),
        accepts_asserted_effort: Some(false),
        discovery_error: None,
    }
}

fn executor_capabilities(accepts_asserted_effort: bool) -> ExecutorCapabilities {
    let effort = EffortCapabilities {
        accepts_asserted_effort: Some(accepts_asserted_effort),
        ..capabilities()
    };
    ExecutorCapabilities {
        version: 1,
        harnesses: std::collections::BTreeMap::from([(
            "codex".to_owned(),
            ExecutorHarnessCapabilities {
                version: effort.harness_version.clone(),
                effort: Some(effort),
            },
        )]),
        discovery_error: None,
    }
}

fn configured(
    directory: &TestDirectory,
    server_url: &str,
    executor: Option<&PathBuf>,
) -> (Config, Client, RunnerConnection) {
    configured_with_overrides(directory, server_url, executor, "")
}

fn configured_with_overrides(
    directory: &TestDirectory,
    server_url: &str,
    executor: Option<&PathBuf>,
    overrides: &str,
) -> (Config, Client, RunnerConnection) {
    let credentials_path = directory.0.join("credentials.toml");
    CredentialStore::at(&credentials_path)
        .save(&RunnerCredentials::new("rnr_cancel", "runner-token"))
        .expect("save runner credentials");
    let executor = executor
        .map(|path| {
            format!(
                "executor = {}\n",
                serde_json::to_string(&vec![path.to_string_lossy().into_owned()])
                    .expect("encode executor")
            )
        })
        .unwrap_or_default();
    let workspace_parent = directory.0.join("workspaces");
    let config = Config::from_toml_str(&format!(
        "[server]\nurl = {server_url:?}\n[runner]\nname = \"cancel-test\"\nexecutor_cwd = \"~\"\n{executor}workspace_parent = {:?}\n{overrides}\n[storage]\ncredentials_file = {:?}\n",
        workspace_parent,
        credentials_path
    ))
    .expect("parse runner config");
    let client =
        Client::with_timeout(server_url, Duration::from_secs(5)).expect("create protocol client");
    let connection = RunnerConnection::connect(&config).expect("load runner credentials");
    (config, client, connection)
}

fn successful_executor(directory: &TestDirectory, name: &str) -> PathBuf {
    let path = directory.0.join(name);
    fs::write(
        &path,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"version":1,"type":"result","status":"completed","exit_code":0,"interrupted":false}'
"#,
    )
    .expect("write executor stub");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("make executor stub executable");
    path
}

fn prepared(config: &Config, client: &Client, assignment: &RunnerAssignment) -> PreparedAssignment {
    let resolved = resolve_assignment(config, client, assignment).expect("resolve issue metadata");
    PreparedAssignment::new(resolved)
}

#[test]
fn cancellation_before_spawn_settles_without_logs_or_finish() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(1);
    let (config, client, connection) = configured(&directory, &server_url, None);
    let assignment = assignment(None);
    let prepared = prepared(&config, &client, &assignment);
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let shutdown = ShutdownSignal::inactive();
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let context = ExecutionContext::new(&shutdown, &active_runs);

    let outcome = execute_assignment_cancellable(
        prepared,
        &connection,
        &client,
        &config.workspace_retention,
        &cancellation,
        &context,
    )
    .expect("cancel before launch");

    assert_eq!(outcome, ExecutionOutcome::Cancelled);
    assert!(
        !directory.0.join("workspaces").exists(),
        "the daemon does not create a workspace"
    );
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
}

#[test]
fn local_timeout_kills_the_harness_and_reports_a_failed_finish() {
    let directory = TestDirectory::new();
    let executor = directory.0.join("slow-executor");
    fs::write(&executor, "#!/bin/sh\nexec sleep 30\n").expect("write executor");
    fs::set_permissions(&executor, fs::Permissions::from_mode(0o755))
        .expect("make executor executable");

    let (server_url, server, _log_seen) = cancellation_server(3);
    let (config, client, connection) = configured(&directory, &server_url, Some(&executor));
    let mut assignment = assignment(None);
    assignment.timeout_minutes = 0;
    let prepared = prepared(&config, &client, &assignment);
    let shutdown = ShutdownSignal::inactive();
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let context = ExecutionContext::new(&shutdown, &active_runs);

    let outcome = execute_assignment_cancellable(
        prepared,
        &connection,
        &client,
        &config.workspace_retention,
        &CancellationToken::default(),
        &context,
    )
    .expect("report local timeout");

    assert_eq!(outcome, ExecutionOutcome::Finished);
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_cancel/logs "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_cancel/finish "));
    let (_, body) = requests[2].split_once("\r\n\r\n").expect("finish body");
    let finish: serde_json::Value = serde_json::from_str(body).expect("decode finish payload");
    assert_eq!(finish["status"], "failed");
    assert!(
        finish["error"]
            .as_str()
            .unwrap()
            .contains("0-minute run timeout")
    );
}

#[test]
fn cancellation_while_running_kills_the_process_group_without_logs_or_finish() {
    let directory = TestDirectory::new();
    let pid_file = directory.0.join("descendant.pid");
    let executor = directory.0.join("slow-executor");
    fs::write(
        &executor,
        format!(
            "#!/bin/sh\n(trap '' TERM; exec sleep 30) &\necho $! > '{}'\nwait\n",
            pid_file.display()
        ),
    )
    .expect("write executor");
    fs::set_permissions(&executor, fs::Permissions::from_mode(0o755))
        .expect("make executor executable");

    let (server_url, server, log_seen) = cancellation_server(2);
    let (config, client, connection) = configured(&directory, &server_url, Some(&executor));
    let assignment = assignment(Some(&pid_file));
    let prepared = prepared(&config, &client, &assignment);
    let cancellation = CancellationToken::default();
    let worker_token = cancellation.clone();
    let worker_retention = config.workspace_retention.clone();
    let worker_shutdown = ShutdownSignal::inactive();
    let worker_active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let worker = thread::spawn(move || {
        let context = ExecutionContext::new(&worker_shutdown, &worker_active_runs);
        execute_assignment_cancellable(
            prepared,
            &connection,
            &client,
            &worker_retention,
            &worker_token,
            &context,
        )
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let descendant = fs::read_to_string(&pid_file)
        .expect("executor stub wrote descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse descendant PID");
    log_seen
        .recv_timeout(Duration::from_secs(3))
        .expect("harness-start log request completed");
    cancellation.cancel();

    assert_eq!(
        worker
            .join()
            .expect("join assignment worker")
            .expect("cancel run"),
        ExecutionOutcome::Cancelled
    );
    assert!(
        !directory.0.join("workspaces").exists(),
        "the stub executor did not create a workspace"
    );
    assert_process_stopped(descendant);
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(
        requests.len(),
        2,
        "only metadata and harness-start logs ship"
    );
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_cancel/logs "));
}

#[test]
fn cancellation_during_metadata_enrichment_does_not_materialize_or_finish() {
    let directory = TestDirectory::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
    let address = listener.local_addr().expect("read fake Tines address");
    let (request_seen_tx, request_seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept issue detail request");
        let request = read_request(&mut stream);
        request_seen_tx.send(()).expect("notify request received");
        release_rx.recv().expect("release delayed issue response");
        respond(
            &mut stream,
            r#"{"id":"iss_cancel","workflow":{"name":"Implementation"}}"#,
        );
        request
    });
    let server_url = format!("http://{address}");
    let (config, client, connection) = configured(&directory, &server_url, None);
    let workspace_parent = directory.0.join("workspaces");
    let cancellation = CancellationToken::default();
    let worker_token = cancellation.clone();
    let worker_config = config.clone();
    let worker_client = client.clone();
    let worker_connection = connection.clone();
    let worker_shutdown = ShutdownSignal::inactive();
    let worker_active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let worker = thread::spawn(move || {
        let context = ExecutionContext::new(&worker_shutdown, &worker_active_runs);
        run_assignment(
            &worker_config,
            &worker_connection,
            &worker_client,
            assignment(None),
            tines_runner_rs::protocol::client::RunLogBuffer::new(),
            &tines_runner_rs::executor_transport::ExecutorTransport::new(
                worker_config.executor.clone(),
                worker_config.executor_cwd.clone(),
            ),
            &tines_runner_rs::executor_capabilities::ExecutorCapabilities::unavailable(
                "test cancellation",
            ),
            &worker_token,
            &context,
        )
    });

    request_seen_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("metadata request started");
    cancellation.cancel();
    release_tx.send(()).expect("release metadata response");
    assert_eq!(
        worker
            .join()
            .expect("join assignment worker")
            .expect("cancel metadata"),
        AssignmentTaskOutcome::Cancelled
    );
    assert!(
        !workspace_parent.exists(),
        "canceled metadata lookup creates no workspace"
    );
    let request = server.join().expect("join fake Tines server");
    assert!(request.starts_with("GET /api/v1/issues/iss_cancel "));
}

#[test]
fn unsupported_executor_declines_before_workspace_materialization() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(1);
    let (config, client, connection) = configured(&directory, &server_url, None);
    let default_executor = tines_runner_rs::executor_transport::ExecutorTransport::new(
        config.executor.clone(),
        config.executor_cwd.clone(),
    );
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = ExecutionContext::new(&shutdown, &active_runs);

    let outcome = run_assignment(
        &config,
        &connection,
        &client,
        assignment(None),
        tines_runner_rs::protocol::client::RunLogBuffer::new(),
        &default_executor,
        &tines_runner_rs::executor_capabilities::ExecutorCapabilities::unavailable(
            "Codex is not installed in the executor",
        ),
        &CancellationToken::default(),
        &context,
    )
    .expect("decline an assignment without harness support");

    assert!(
        matches!(outcome, AssignmentTaskOutcome::Declined(reason) if reason.contains("does not verify support"))
    );
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(
        !directory.0.join("workspaces").exists(),
        "unsupported harnesses do not create workspaces"
    );
}

#[test]
fn executor_effort_capabilities_authorize_effort_without_a_legacy_launcher() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(3);
    let executor = successful_executor(&directory, "effort-executor");
    let (config, client, connection) = configured(&directory, &server_url, Some(&executor));
    let default_executor = tines_runner_rs::executor_transport::ExecutorTransport::new(
        config.executor.clone(),
        config.executor_cwd.clone(),
    );
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = ExecutionContext::new(&shutdown, &active_runs);
    let mut assignment = assignment(None);
    assignment.effort = Some(RunnerAssignmentEffort {
        version: 1,
        value: "high".to_owned(),
        capability_digest: Some(capabilities().catalog_digest.clone()),
        verification: Some("asserted".to_owned()),
    });

    let outcome = run_assignment(
        &config,
        &connection,
        &client,
        assignment,
        tines_runner_rs::protocol::client::RunLogBuffer::new(),
        &default_executor,
        &executor_capabilities(true),
        &CancellationToken::default(),
        &context,
    )
    .expect("execute using the configured executor effort report");

    assert_eq!(outcome, AssignmentTaskOutcome::Finished);
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_cancel/logs "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_cancel/finish "));
    let (_, finish_body) = requests[2].split_once("\r\n\r\n").expect("finish body");
    let finish: serde_json::Value = serde_json::from_str(finish_body).expect("decode finish");
    assert_eq!(finish["status"], "completed");
}

#[test]
fn codex_to_custom_override_accepts_effort_without_codex_effort_capabilities() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(3);
    let executor = successful_executor(&directory, "custom-override-executor");
    let (config, client, connection) = configured_with_overrides(
        &directory,
        &server_url,
        Some(&executor),
        "[[override]]\nproject = \"Tines\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nrunner_type = \"custom\"\ncustom_command = [\"custom-command\"]\n",
    );
    assert_eq!(
        config.runner_type,
        tines_runner_rs::config::RunnerType::Codex
    );
    let default_executor = tines_runner_rs::executor_transport::ExecutorTransport::new(
        config.executor.clone(),
        config.executor_cwd.clone(),
    );
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = ExecutionContext::new(&shutdown, &active_runs);
    let mut assignment = assignment(None);
    assignment.effort = Some(RunnerAssignmentEffort {
        version: 1,
        value: "high".to_owned(),
        capability_digest: Some(capabilities().catalog_digest),
        verification: Some("asserted".to_owned()),
    });
    let custom_capabilities = ExecutorCapabilities {
        version: 1,
        harnesses: std::collections::BTreeMap::from([(
            "custom".to_owned(),
            ExecutorHarnessCapabilities {
                version: "custom-command 1.0".to_owned(),
                effort: None,
            },
        )]),
        discovery_error: None,
    };

    let outcome = run_assignment(
        &config,
        &connection,
        &client,
        assignment,
        tines_runner_rs::protocol::client::RunLogBuffer::new(),
        &default_executor,
        &custom_capabilities,
        &CancellationToken::default(),
        &context,
    )
    .expect("execute a Codex base config resolved to custom");

    assert_eq!(outcome, AssignmentTaskOutcome::Finished);
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_cancel/logs "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_cancel/finish "));
}

#[test]
fn custom_override_still_requires_verified_custom_harness_support() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(1);
    let executor = successful_executor(&directory, "unsupported-custom-executor");
    let (config, client, connection) = configured_with_overrides(
        &directory,
        &server_url,
        Some(&executor),
        "[[override]]\nproject = \"Tines\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nrunner_type = \"custom\"\ncustom_command = [\"custom-command\"]\n",
    );
    let default_executor = tines_runner_rs::executor_transport::ExecutorTransport::new(
        config.executor.clone(),
        config.executor_cwd.clone(),
    );
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = ExecutionContext::new(&shutdown, &active_runs);
    let mut assignment = assignment(None);
    assignment.effort = Some(RunnerAssignmentEffort {
        version: 1,
        value: "high".to_owned(),
        capability_digest: Some(capabilities().catalog_digest),
        verification: Some("asserted".to_owned()),
    });

    let outcome = run_assignment(
        &config,
        &connection,
        &client,
        assignment,
        tines_runner_rs::protocol::client::RunLogBuffer::new(),
        &default_executor,
        &executor_capabilities(true),
        &CancellationToken::default(),
        &context,
    )
    .expect("decline a custom run without custom capability support");

    assert!(matches!(
        outcome,
        AssignmentTaskOutcome::Declined(reason)
            if reason == "configured executor does not verify support for the custom harness"
    ));
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(!directory.0.join("workspaces").exists());
}

#[test]
fn codex_effort_assignments_still_require_verified_effort_support() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(1);
    let (config, client, connection) = configured(&directory, &server_url, None);
    let default_executor = tines_runner_rs::executor_transport::ExecutorTransport::new(
        config.executor.clone(),
        config.executor_cwd.clone(),
    );
    let active_runs =
        ActiveRunStore::open(directory.0.join("active-runs.json")).expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = ExecutionContext::new(&shutdown, &active_runs);
    let mut assignment = assignment(None);
    assignment.effort = Some(RunnerAssignmentEffort {
        version: 1,
        value: "high".to_owned(),
        capability_digest: Some(capabilities().catalog_digest),
        verification: Some("asserted".to_owned()),
    });

    let outcome = run_assignment(
        &config,
        &connection,
        &client,
        assignment,
        tines_runner_rs::protocol::client::RunLogBuffer::new(),
        &default_executor,
        &executor_capabilities(false),
        &CancellationToken::default(),
        &context,
    )
    .expect("decline effort the Codex report cannot verify");

    assert!(matches!(
        outcome,
        AssignmentTaskOutcome::Declined(reason)
            if reason == "the assigned model effort support could not be verified"
    ));
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
}

#[cfg(target_os = "linux")]
fn assert_process_stopped(process_id: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::read_to_string(format!("/proc/{process_id}/stat")) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(") ")
                    .expect("valid proc stat record")
                    .1
                    .chars()
                    .next()
                    .expect("process state");
                if state == 'Z' {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("inspect descendant process: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "descendant process remained alive"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn assert_process_stopped(process_id: u32) {
    let mut command = std::process::Command::new("kill");
    command.args(["-0", &process_id.to_string()]);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !command
            .status()
            .expect("probe descendant process")
            .success()
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("descendant process remained alive");
}
