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
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::recovery::ActiveRunStore;
use tines_runner_rs::runner::RunnerConnection;
use tines_runner_rs::shutdown::ShutdownSignal;
use tines_runner_rs::workspace::MaterializedWorkspace;

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
        catalog_digest: "empty".to_owned(),
        models: Vec::new(),
        accepts_asserted_effort: Some(false),
        discovery_error: None,
    }
}

fn configured(
    directory: &TestDirectory,
    server_url: &str,
    wrapper: Option<&PathBuf>,
) -> (Config, Client, RunnerConnection) {
    let credentials_path = directory.0.join("credentials.toml");
    CredentialStore::at(&credentials_path)
        .save(&RunnerCredentials::new("rnr_cancel", "runner-token"))
        .expect("save runner credentials");
    let wrapper = wrapper
        .map(|path| {
            format!(
                "wrapper = {}\n",
                serde_json::to_string(&vec![path.to_string_lossy().into_owned()])
                    .expect("encode wrapper")
            )
        })
        .unwrap_or_default();
    let workspace_parent = directory.0.join("workspaces");
    let config = Config::from_toml_str(&format!(
        "[server]\nurl = {server_url:?}\n[runner]\nname = \"cancel-test\"\n{wrapper}workspace_parent = {:?}\n[storage]\ncredentials_file = {:?}\n",
        workspace_parent,
        credentials_path
    ))
    .expect("parse runner config");
    let client =
        Client::with_timeout(server_url, Duration::from_secs(5)).expect("create protocol client");
    let connection = RunnerConnection::connect(&config).expect("load runner credentials");
    (config, client, connection)
}

fn prepared(config: &Config, client: &Client, assignment: &RunnerAssignment) -> PreparedAssignment {
    let resolved = resolve_assignment(config, client, assignment).expect("resolve issue metadata");
    let workspace = MaterializedWorkspace::create(
        &resolved.resolution().config.workspace_parent,
        resolved.assignment(),
        &config.server_url,
    )
    .expect("create workspace");
    PreparedAssignment::new(resolved, workspace)
}

#[test]
fn cancellation_before_spawn_cleans_the_workspace_without_logs_or_finish() {
    let directory = TestDirectory::new();
    let (server_url, server, _log_seen) = cancellation_server(1);
    let (config, client, connection) = configured(&directory, &server_url, None);
    let assignment = assignment(None);
    let prepared = prepared(&config, &client, &assignment);
    let workspace_path = prepared.workspace().path().to_path_buf();
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
        &capabilities(),
        &config.workspace_retention,
        &cancellation,
        &context,
    )
    .expect("cancel before launch");

    assert_eq!(outcome, ExecutionOutcome::Cancelled);
    assert!(!workspace_path.exists(), "canceled workspace is cleaned");
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
}

#[test]
fn local_timeout_kills_the_harness_and_reports_a_failed_finish() {
    let directory = TestDirectory::new();
    let wrapper = directory.0.join("slow-wrapper");
    fs::write(&wrapper, "#!/bin/sh\nexec sleep 30\n").expect("write wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
        .expect("make wrapper executable");

    let (server_url, server, _log_seen) = cancellation_server(4);
    let (config, client, connection) = configured(&directory, &server_url, Some(&wrapper));
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
        &capabilities(),
        &config.workspace_retention,
        &CancellationToken::default(),
        &context,
    )
    .expect("report local timeout");

    assert_eq!(outcome, ExecutionOutcome::Finished);
    let requests = server.join().expect("join fake Tines server");
    assert_eq!(requests.len(), 4);
    assert!(requests[0].starts_with("GET /api/v1/issues/iss_cancel "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_cancel/logs "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_cancel/logs "));
    assert!(requests[3].starts_with("POST /api/v1/runs/arun_cancel/finish "));
    let (_, body) = requests[3].split_once("\r\n\r\n").expect("finish body");
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
    let wrapper = directory.0.join("slow-wrapper");
    fs::write(
        &wrapper,
        "#!/bin/sh\n(trap '' TERM; exec sleep 30) &\necho $! > \"$PID_FILE\"\nwait\n",
    )
    .expect("write wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
        .expect("make wrapper executable");

    let (server_url, server, log_seen) = cancellation_server(2);
    let (config, client, connection) = configured(&directory, &server_url, Some(&wrapper));
    let assignment = assignment(Some(&pid_file));
    let prepared = prepared(&config, &client, &assignment);
    let workspace_path = prepared.workspace().path().to_path_buf();
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
            &capabilities(),
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
        .expect("wrapper wrote descendant PID")
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
    assert!(!workspace_path.exists(), "canceled workspace is cleaned");
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
            &capabilities(),
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
