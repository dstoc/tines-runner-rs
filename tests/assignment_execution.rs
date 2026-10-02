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
use tines_runner_rs::effort::EffortCapabilities;
use tines_runner_rs::execution::execute_assignment;
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::runner::RunnerConnection;
use tines_runner_rs::workspace::MaterializedWorkspace;

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
                respond(
                    &mut stream,
                    200,
                    r#"{"status":"running","log_bytes_dropped":0,"log_seq":1}"#,
                );
            } else if request.starts_with("POST /api/v1/runs/arun_finish_runtime/finish ") {
                finish_requests += 1;
                let workspaces = fs::read_dir(&workspace_parent)
                    .expect("read workspace parent")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("list workspaces");
                assert_eq!(workspaces.len(), 1, "workspace remains through finish");
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

fn capabilities() -> EffortCapabilities {
    EffortCapabilities {
        version: 1,
        daemon_version: "test-runner".to_owned(),
        harness: "codex".to_owned(),
        harness_version: "stub".to_owned(),
        catalog_digest: "empty".to_owned(),
        models: Vec::new(),
        accepts_asserted_effort: Some(false),
        discovery_error: None,
    }
}

fn run_case(exit_code: i32, expected_status: &str) {
    let directory = TestDirectory::new();
    let fixture_path = directory.0.join("codex-output.jsonl");
    fs::write(&fixture_path, include_str!("fixtures/codex-stream.jsonl"))
        .expect("write Codex fixture");
    let script_path = directory.0.join("stub-codex-wrapper");
    let script = format!(
        "#!/bin/sh\ncat '{}'\nif [ '{}' -ne 0 ]; then echo \"fixture failed: $TINES_API_KEY\" >&2; fi\nexit '{}'\n",
        fixture_path.display(),
        exit_code,
        exit_code
    );
    fs::write(&script_path, script).expect("write stub Codex wrapper");
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .expect("make stub wrapper executable");

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
    let wrapper = serde_json::to_string(&vec![script_path.to_string_lossy().into_owned()])
        .expect("encode wrapper config");
    let workspace_parent = serde_json::to_string(&workspace_parent.to_string_lossy().as_ref())
        .expect("encode workspace path");
    let config = Config::from_toml_str(&format!(
        "[server]\nurl = {server_url:?}\n[runner]\nname = \"finish-test\"\nwrapper = {wrapper}\nworkspace_parent = {workspace_parent}\n[storage]\ncredentials_file = {}\n",
        serde_json::to_string(&credentials_path.to_string_lossy().as_ref())
            .expect("encode credentials path")
    ))
    .expect("parse runner config");
    let connection = RunnerConnection::connect(&config).expect("load runner credentials");

    let protocol_assignment: RunnerAssignment =
        serde_json::from_value(assignment()).expect("decode assignment");
    let resolved = resolve_assignment(&config, &client, &protocol_assignment)
        .expect("resolve assignment metadata");
    let workspace = MaterializedWorkspace::create(
        &resolved.resolution().config.workspace_parent,
        resolved.assignment(),
        &config.server_url,
    )
    .expect("create assignment workspace");
    let workspace_path = workspace.path().to_path_buf();
    let prepared = PreparedAssignment::new(resolved, workspace);

    execute_assignment(prepared, &connection, &client, &capabilities())
        .expect("execute and settle assignment");
    assert!(
        !workspace_path.exists(),
        "workspace is removed after finish"
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
    let (_, log_body) = requests[2].split_once("\r\n\r\n").expect("log body");
    let log: Value = serde_json::from_str(log_body).expect("decode log payload");
    assert!(!log["chunk"].as_str().unwrap().contains("issue-run-key"));
    if expected_status == "failed" {
        assert!(log["chunk"].as_str().unwrap().contains("[REDACTED]"));
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
fn real_executor_reports_completed_run_and_cleans_after_finish_retry() {
    run_case(0, "completed");
}

#[test]
fn real_executor_reports_failed_run_with_usage_and_cleans_after_finish_retry() {
    run_case(7, "failed");
}
