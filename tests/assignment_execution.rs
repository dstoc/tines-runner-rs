#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use tines_runner_rs::assignment::{PreparedAssignment, resolve_assignment};
use tines_runner_rs::config::Config;
use tines_runner_rs::credentials::{CredentialStore, RunnerCredentials};
use tines_runner_rs::execution::execute_assignment;
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::recovery::ActiveRunStore;
use tines_runner_rs::runner::RunnerConnection;
use tines_runner_rs::shutdown::ShutdownSignal;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tines-runner-finish-execution-{}-{id}",
            std::process::id()
        ));
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
        let count = stream.read(&mut chunk).expect("read request");
        assert_ne!(count, 0, "client closed before completing request");
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

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let label = if status == 200 { "OK" } else { "Unavailable" };
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write fake Tines response");
}

fn fake_server(workspace_parent: PathBuf) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
    let address = listener.local_addr().expect("read fake server address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut finish_requests = 0;
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().expect("accept Tines request");
            let request = read_request(&mut stream);
            if request.starts_with("GET /api/v1/issues/iss_finish_runtime ") {
                respond(
                    &mut stream,
                    200,
                    r#"{"id":"iss_finish_runtime","workflow":{"name":"Implementation"}}"#,
                );
            } else if request.starts_with("POST /api/v1/runs/arun_finish_runtime/logs ") {
                let (_, body) = request.split_once("\r\n\r\n").expect("log body");
                let log: Value = serde_json::from_str(body).expect("decode log payload");
                let seq = log["seq"].as_u64().expect("log sequence");
                let response =
                    format!("{{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":{seq}}}");
                respond(&mut stream, 200, &response);
            } else if request.starts_with("POST /api/v1/runs/arun_finish_runtime/finish ") {
                finish_requests += 1;
                let workspaces = fs::read_dir(&workspace_parent)
                    .expect("read workspace parent")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("list workspaces");
                assert!(
                    workspaces.is_empty(),
                    "the daemon does not materialize an executor workspace"
                );
                if finish_requests == 1 {
                    respond(
                        &mut stream,
                        503,
                        r#"{"error":{"code":"unavailable","message":"retry"}}"#,
                    );
                } else {
                    respond(
                        &mut stream,
                        200,
                        r#"{"id":"arun_finish_runtime","status":"completed"}"#,
                    );
                }
            } else {
                panic!("unexpected Tines request: {request}");
            }
            requests.push(request);
        }
        requests
    });
    (format!("http://{address}"), server)
}

fn assignment() -> Value {
    json!({
        "run": {
            "id": "arun_finish_runtime",
            "issue_id": "iss_finish_runtime",
            "issue_ref": {
                "project_name": "Tines",
                "number": 14,
                "title": "Finish reporting"
            },
            "state_at_start_name": "Implement",
            "model": "gpt-5.1-codex"
        },
        "prompt": "run the fixture",
        "bundle": {"skills": [], "repos": []},
        "run_key": "issue-run-key",
        "timeout_minutes": 5,
        "env": []
    })
}

fn run_case(exit_code: i32, expected_status: &str) {
    let directory = TestDirectory::new();
    let fixture_path = directory.0.join("executor-events.jsonl");
    let script_path = directory.0.join("stub-executor");
    let status = if exit_code == 0 {
        "completed"
    } else {
        "failed"
    };
    let error = (exit_code != 0)
        .then(|| format!("executor exited with code {exit_code}; fixture failed: issue-run-key"));
    let evidence_status = if exit_code == 0 {
        "complete"
    } else {
        "incomplete_attempt"
    };
    let events = [
        json!({"version":1,"type":"log","stream":"stdout","message":"[session] started; fixture failed: issue-run-key"}),
        json!({"version":1,"type":"session","provider":"codex","id":"01a09e68-24d8-78d3-8bc7-67037a0cd7de"}),
        json!({"version":1,"type":"usage","input_tokens":8913,"output_tokens":1980,"cache_read_tokens":101888,"cache_write_tokens":0}),
        json!({
            "version":1,
            "type":"result",
            "status":status,
            "exit_code":exit_code,
            "error":error,
            "provider_session_id":"01a09e68-24d8-78d3-8bc7-67037a0cd7de",
            "usage":{"input_tokens":8913,"output_tokens":1980,"cache_read_tokens":101888,"cache_write_tokens":0},
            "pricing_evidence":{
                "provider":"codex",
                "version":1,
                "payload":{
                    "version":1,
                    "harness":"codex",
                    "model":"gpt-5.1-codex",
                    "identity_source":"launch_argument",
                    "usage_scope":"thread_total",
                    "session_mode":"cold",
                    "normalization":"codex-jsonl-v1",
                    "raw_usage":{
                        "input_tokens":110801,
                        "cached_input_tokens":101888,
                        "cache_write_input_tokens":0,
                        "output_tokens":1980
                    },
                    "model_rerouted":false,
                    "measurement_status":evidence_status,
                    "terminal_snapshots":1,
                    "request_context":{
                        "version":1,
                        "normalization":"codex-rollout-delta-v1",
                        "status":"unavailable",
                        "reason":"not_applicable"
                    }
                }
            },
            "interrupted":false
        }),
    ];
    let mut fixture = String::new();
    for event in events {
        fixture.push_str(&serde_json::to_string(&event).expect("encode executor event"));
        fixture.push('\n');
    }
    fs::write(&fixture_path, fixture).expect("write executor event fixture");
    let script = format!(
        "#!/bin/sh\ncat > '{}.request'\ncat '{}'\nif [ '{}' -ne 0 ]; then echo 'executor diagnostic: issue-run-key' >&2; fi\n",
        directory.0.join("capture").display(),
        fixture_path.display(),
        exit_code
    );
    fs::write(&script_path, script).expect("write stub executor");
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .expect("make stub executor executable");

    let workspace_parent = directory.0.join("workspaces");
    fs::create_dir_all(&workspace_parent).expect("create workspace parent");
    let (server_url, server) = fake_server(workspace_parent.clone());
    let client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create protocol client");
    let credentials_path = directory.0.join("credentials.toml");
    CredentialStore::at(&credentials_path)
        .save(&RunnerCredentials::new(
            "rnr_finish_runtime",
            "runner-token",
        ))
        .expect("write runner credentials");
    let executor = serde_json::to_string(&vec![script_path.to_string_lossy().into_owned()])
        .expect("encode executor config");
    let workspace_parent_value =
        serde_json::to_string(&workspace_parent.to_string_lossy().as_ref())
            .expect("encode workspace path");
    let config = Config::from_toml_str(&format!(
        "[server]\nurl = {server_url:?}\n[runner]\nname = \"finish-test\"\nexecutor_cwd = \"~\"\nexecutor = {executor}\nworkspace_parent = {workspace_parent_value}\n[storage]\ncredentials_file = {}\n",
        serde_json::to_string(&credentials_path.to_string_lossy().as_ref())
            .expect("encode credentials path")
    ))
    .expect("parse runner config");
    let connection = RunnerConnection::connect(&config).expect("load runner credentials");

    let protocol_assignment: RunnerAssignment =
        serde_json::from_value(assignment()).expect("decode assignment");
    let resolved = resolve_assignment(&config, &client, &protocol_assignment)
        .expect("resolve assignment metadata");
    let prepared = PreparedAssignment::new(resolved);
    let active_runs =
        ActiveRunStore::open(config.credentials_file.with_file_name("active-runs.json"))
            .expect("load active-run state");
    let shutdown = ShutdownSignal::inactive();
    let context = tines_runner_rs::execution::ExecutionContext::new(&shutdown, &active_runs);

    execute_assignment(
        prepared,
        &connection,
        &client,
        &config.workspace_retention,
        &context,
    )
    .expect("execute and settle assignment");
    assert!(active_runs.records().is_empty());
    assert_eq!(fs::read_dir(&workspace_parent).unwrap().count(), 0);
    let execution_request = fs::read(directory.0.join("capture.request"))
        .expect("read request delivered to generic executor");
    let execution_request: Value =
        serde_json::from_slice(&execution_request).expect("decode execution request");
    assert_eq!(execution_request["assignment"]["run_key"], "issue-run-key");
    assert!(execution_request.get("runner_token").is_none());
    assert!(
        !execution_request.to_string().contains("runner-token"),
        "long-lived runner credentials do not cross the executor boundary"
    );

    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 5);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_finish_runtime "));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer issue-run-key")
    );
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_finish_runtime/logs "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_finish_runtime/logs "));
    let (_, first_log_body) = requests[1].split_once("\r\n\r\n").expect("first log body");
    let first_log: Value = serde_json::from_str(first_log_body).expect("decode first log");
    assert_eq!(first_log["seq"], 1);
    assert_eq!(first_log["chunk"], "");
    let (_, log_body) = requests[2].split_once("\r\n\r\n").expect("log body");
    let log: Value = serde_json::from_str(log_body).expect("decode log payload");
    let chunk = log["chunk"].as_str().unwrap();
    assert_eq!(log["seq"], 2);
    assert!(chunk.contains("[session] started"));
    assert!(!chunk.contains("issue-run-key"));
    if expected_status == "failed" {
        assert!(chunk.contains("[REDACTED]"));
    }
    let finish_request = &requests[4];
    assert!(finish_request.starts_with("POST /api/v1/runs/arun_finish_runtime/finish "));
    assert!(
        finish_request
            .to_ascii_lowercase()
            .contains("authorization: bearer runner-token")
    );
    let (_, body) = finish_request.split_once("\r\n\r\n").expect("finish body");
    let finish: Value = serde_json::from_str(body).expect("decode finish payload");
    assert_eq!(finish["status"], expected_status);
    assert_eq!(
        finish["provider_session_id"],
        "01a09e68-24d8-78d3-8bc7-67037a0cd7de"
    );
    assert_eq!(
        finish["usage"],
        json!({
            "input_tokens": 8913,
            "output_tokens": 1980,
            "cache_read_tokens": 101888,
            "cache_write_tokens": 0
        })
    );
    assert_eq!(finish["pricing_evidence"]["model"], "gpt-5.1-codex");
    assert_eq!(
        finish["pricing_evidence"]["raw_usage"]["input_tokens"],
        110801
    );
    assert_eq!(
        finish["pricing_evidence"]["request_context"]["reason"],
        "not_applicable"
    );
    assert_eq!(
        finish["pricing_evidence"]["measurement_status"],
        if expected_status == "failed" {
            "incomplete_attempt"
        } else {
            "complete"
        }
    );
    if expected_status == "failed" {
        assert!(finish["error"].as_str().unwrap().contains("code 7"));
        assert!(finish["error"].as_str().unwrap().contains("fixture failed"));
        assert!(finish["error"].as_str().unwrap().contains("[REDACTED]"));
        assert!(!finish["error"].as_str().unwrap().contains("issue-run-key"));
    } else {
        assert!(finish.get("error").is_none());
    }
}

#[test]
fn stub_executor_reports_completed_run_and_cleans_after_finish_retry() {
    run_case(0, "completed");
}

#[test]
fn stub_executor_reports_failed_run_with_usage_and_cleans_after_finish_retry() {
    run_case(7, "failed");
}
