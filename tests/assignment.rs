use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tines_runner_rs::assignment::{PreparedAssignment, resolve_assignment};
use tines_runner_rs::config::Config;
use tines_runner_rs::credentials::{CredentialStore, RunnerCredentials};
use tines_runner_rs::executor_transport::execution_request;
use tines_runner_rs::poll::PollLoop;
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::runner::RunnerConnection;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tines-runner-assignment-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn credentials_path(&self) -> PathBuf {
        self.0.join("credentials.toml")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assignment() -> RunnerAssignment {
    serde_json::from_value(serde_json::json!({
        "run": {
            "id": "arun_assignment_test",
            "issue_id": "iss_assignment_test",
            "issue_ref": {
                "project_name": "Tines",
                "number": 7,
                "title": "Resolve assignment config"
            },
            "state_at_start_name": "Implement"
        },
        "prompt": "work",
        "bundle": {},
        "run_key": "ephemeral-run-key",
        "timeout_minutes": 30
    }))
    .expect("deserialize assignment")
}

fn config(overrides: &str) -> Config {
    Config::from_toml_str(&format!(
        r#"
            [server]
            url = "http://127.0.0.1:1"
            [runner]
            name = "assignment-test"
            executor_cwd = "~"
            wrapper = ["base-wrapper"]
            {overrides}
        "#
    ))
    .expect("parse runner config")
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
            return String::from_utf8(request).expect("request is UTF-8");
        }
    }
}

fn issue_server_with_response(
    count: usize,
    status: u16,
    body: &'static str,
) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (mut stream, _) = listener.accept().expect("accept request");
                let request = read_request(&mut stream);
                write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write issue response");
                request
            })
            .collect()
    });
    (format!("http://{address}"), server)
}

fn issue_server(count: usize) -> (String, JoinHandle<Vec<String>>) {
    issue_server_with_response(
        count,
        200,
        r#"{"id":"iss_assignment_test","workflow":{"name":"Implementation"}}"#,
    )
}

fn assignment_poll_server() -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        let (mut poll_stream, _) = listener.accept().expect("accept runner poll");
        let poll_request = read_request(&mut poll_stream);
        let poll_response = r#"{"assignments":[{"run":{"id":"arun_queued_assignment","issue_id":"iss_assignment_test","issue_ref":{"project_name":"Tines","number":7,"title":"Resolve assignment config"},"state_at_start_name":"Implement"},"prompt":"queued prompt","bundle":{"skills":[],"repos":[]},"run_key":"ephemeral-run-key","timeout_minutes":30}],"cancels":[]}"#;
        write!(
            poll_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{poll_response}",
            poll_response.len()
        )
        .expect("write poll response");

        let (mut issue_stream, _) = listener.accept().expect("accept issue detail request");
        let issue_request = read_request(&mut issue_stream);
        let issue_response = r#"{"id":"iss_assignment_test","workflow":{"name":"Implementation"}}"#;
        write!(
            issue_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{issue_response}",
            issue_response.len()
        )
        .expect("write issue detail response");
        vec![poll_request, issue_request]
    });
    (format!("http://{address}"), server)
}

#[test]
fn poll_queue_retains_resolved_context_config_and_original_assignment() {
    let directory = TestDirectory::new();
    let store = CredentialStore::at(directory.credentials_path());
    store
        .save(&RunnerCredentials::new(
            "rnr_assignment_test",
            "runner-token",
        ))
        .expect("store runner token");
    let (server_url, server) = assignment_poll_server();
    let config = Config::from_toml_str(&format!(
        r#"
            [server]
            url = "{server_url}"
            [runner]
            name = "assignment-test"
            executor_cwd = "~"
            wrapper = ["base-wrapper"]
            workspace_parent = {:?}
            [[override]]
            project = "TINES"
            workflow = "IMPLEMENTATION"
            state = "IMPLEMENT"
            wrapper = ["combined-wrapper"]
            [storage]
            credentials_file = {:?}
        "#,
        directory.0.join("workspaces"),
        store.path()
    ))
    .expect("parse runner config");
    let connection = RunnerConnection::connect(&config).expect("load runner connection");
    let issue_client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let mut poller = PollLoop::new(connection, &config);
    let polls = std::cell::Cell::new(0);
    poller
        .run_with(
            |response, state| {
                for assignment in &response.assignments {
                    let resolved = resolve_assignment(&config, &issue_client, assignment)
                        .expect("resolve assignment before queueing");
                    let workspace = tines_runner_rs::workspace::MaterializedWorkspace::create(
                        &resolved.resolution().config.workspace_parent,
                        resolved.assignment(),
                        &config.server_url,
                    )
                    .expect("materialize assignment workspace");
                    state.queue_assignment(PreparedAssignment::new(resolved, workspace));
                }
                polls.set(polls.get() + 1);
            },
            || polls.get() == 0,
            |_| panic!("one poll should finish without sleeping"),
        )
        .expect("poll and resolve assignment");

    let requests = server.join().expect("join mock Tines server");
    assert!(requests[0].starts_with("POST /api/v1/runners/rnr_assignment_test/poll HTTP/1.1"));
    assert!(requests[1].starts_with("GET /api/v1/issues/iss_assignment_test HTTP/1.1"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer ephemeral-run-key")
    );

    let queued = poller
        .state()
        .pending_assignments()
        .next()
        .cloned()
        .expect("resolved assignment remains queued");
    assert_eq!(queued.assignment().run.id, "arun_queued_assignment");
    assert_eq!(queued.assignment().prompt, "queued prompt");
    assert_eq!(queued.context().project(), "Tines");
    assert_eq!(queued.context().workflow(), "Implementation");
    assert_eq!(queued.context().state(), "Implement");
    assert_eq!(queued.resolution().config.wrapper, ["combined-wrapper"]);
    assert_eq!(queued.resolution().matching_overrides(), [0]);
    assert!(!format!("{queued:?}").contains("ephemeral-run-key"));

    let claimed = poller
        .state_mut()
        .take_assignment("arun_queued_assignment")
        .expect("executor can take resolved assignment");
    assert_eq!(claimed.context(), queued.context());
    assert_eq!(claimed.resolution(), queued.resolution());
    assert_eq!(claimed.assignment().run_key, "ephemeral-run-key");
}

#[test]
fn execution_request_keeps_workspace_and_retention_in_executor_environment() {
    let (server_url, server) = issue_server(1);
    let config = Config::from_toml_str(
        r#"[server]
url = "http://127.0.0.1:1"
[runner]
name = "execution-request-test"
executor = ["docker", "run", "--rm", "-i", "runner-image"]
executor_cwd = "/host/daemon"
workspace_parent = "/executor/workspaces"
[storage]
keep_workspaces = "failed"
keep_workspaces_for_hours = 36
keep_workspaces_max = 17
"#,
    )
    .expect("parse executor request configuration");
    let client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let resolved =
        resolve_assignment(&config, &client, &assignment()).expect("resolve assignment metadata");
    let request = execution_request(&resolved, &server_url, &config.workspace_retention);
    server.join().expect("issue detail request");

    assert_eq!(resolved.resolution().config.executor[0], "docker");
    assert_eq!(
        resolved.resolution().config.executor_cwd,
        PathBuf::from("/host/daemon")
    );
    assert_eq!(request.execution.harness, "codex");
    assert_eq!(
        request.execution.workspace.parent,
        Some(PathBuf::from("/executor/workspaces"))
    );
    assert_eq!(
        request.execution.retention.mode,
        tines_runner_rs::config::RetentionMode::Failed
    );
    assert_eq!(request.execution.retention.max_age_hours, 36);
    assert_eq!(request.execution.retention.max_count, 17);
    let serialized = serde_json::to_value(request).expect("serialize executor request");
    assert!(serialized.get("executor_cwd").is_none());
    assert!(serialized["execution"].get("executor_cwd").is_none());
}

#[test]
fn execution_request_preserves_executor_tilde_paths_and_omits_its_default() {
    let (server_url, server) = issue_server(3);
    let cases = [
        (
            "workspace_parent = \"~/work/base\"",
            Some(PathBuf::from("~/work/base")),
        ),
        (
            "[[override]]\nproject = \"Tines\"\nworkspace_parent = \"~/work/payments\"",
            Some(PathBuf::from("~/work/payments")),
        ),
        ("", None),
    ];

    for (workspace_config, expected_parent) in cases {
        let config = Config::from_toml_str(&format!(
            "[server]\nurl = \"http://127.0.0.1:1\"\n[runner]\nname = \"executor-path-test\"\nexecutor_cwd = \"/host/daemon\"\n{workspace_config}\n"
        ))
        .expect("parse executor workspace configuration");
        let client =
            Client::with_timeout(&server_url, Duration::from_secs(5)).expect("issue client");
        let resolved = resolve_assignment(&config, &client, &assignment())
            .expect("resolve assignment metadata");
        let request = execution_request(&resolved, &server_url, &config.workspace_retention);
        assert_eq!(request.execution.workspace.parent, expected_parent);
    }

    server.join().expect("issue detail requests");
}

#[test]
fn workspace_materialization_failure_finishes_only_the_assignment() {
    let directory = TestDirectory::new();
    let store = CredentialStore::at(directory.credentials_path());
    store
        .save(&RunnerCredentials::new(
            "rnr_materialization_test",
            "runner-token",
        ))
        .expect("store runner token");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        let responses = [
            (
                r#"{"assignments":[{"run":{"id":"arun_materialization_failure","issue_id":"iss_assignment_test","issue_ref":{"project_name":"Tines","number":7,"title":"Workspace"},"state_at_start_name":"Implement"},"prompt":"work","bundle":{},"run_key":"ephemeral-run-key","timeout_minutes":30}],"cancels":[]}"#,
                "poll",
            ),
            (
                r#"{"id":"iss_assignment_test","workflow":{"name":"Implementation"}}"#,
                "issue",
            ),
            (
                r#"{"id":"arun_materialization_failure","status":"failed"}"#,
                "finish",
            ),
        ];
        responses
            .into_iter()
            .map(|(body, _)| {
                let (mut stream, _) = listener.accept().expect("accept Tines request");
                let request = read_request(&mut stream);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write response");
                request
            })
            .collect::<Vec<_>>()
    });
    let server_url = format!("http://{address}");
    let config = Config::from_toml_str(&format!(
        r#"
            [server]
            url = "{server_url}"
            [runner]
            name = "materialization-test"
            executor_cwd = "~"
            workspace_parent = {:?}
            [storage]
            credentials_file = {:?}
        "#,
        directory.0.join("workspaces"),
        store.path()
    ))
    .expect("parse runner config");
    let connection = RunnerConnection::connect(&config).expect("load runner connection");
    let issue_client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let mut poller = PollLoop::new(connection, &config);
    let polls = std::cell::Cell::new(0);
    poller
        .run_with(
            |response, state| {
                for assignment in &response.assignments {
                    let resolved = resolve_assignment(&config, &issue_client, assignment)
                        .expect("resolve assignment metadata");
                    let error = tines_runner_rs::workspace::MaterializedWorkspace::create(
                        &resolved.resolution().config.workspace_parent,
                        resolved.assignment(),
                        &config.server_url,
                    )
                    .expect_err("fixture bundle is missing workspace data");
                    state.fail_assignment(assignment.run.id.clone(), error.to_string());
                }
                polls.set(polls.get() + 1);
            },
            || polls.get() == 0,
            |_| panic!("one poll should finish without sleeping"),
        )
        .expect("report assignment failure");

    let requests = server.join().expect("join mock Tines server");
    assert!(requests[0].starts_with("POST /api/v1/runners/rnr_materialization_test/poll "));
    assert!(requests[1].starts_with("GET /api/v1/issues/iss_assignment_test "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_materialization_failure/finish "));
    assert!(requests[2].contains("\"status\":\"failed\""));
    assert!(
        requests[2].contains("\"error\":\"assignment bundle has invalid skills or repositories\"")
    );
    assert!(
        requests[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer runner-token")
    );
}

#[test]
fn issue_workflow_and_assignment_snapshot_select_case_insensitive_overrides() {
    let (server_url, server) = issue_server(4);
    let client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let assignment = assignment();
    let cases = [
        (
            "project = \"tInEs\"\nwrapper = [\"project-wrapper\"]",
            "project-wrapper",
        ),
        (
            "workflow = \"iMpLeMeNtAtIoN\"\nwrapper = [\"workflow-wrapper\"]",
            "workflow-wrapper",
        ),
        (
            "state = \"iMpLeMeNt\"\nwrapper = [\"state-wrapper\"]",
            "state-wrapper",
        ),
        (
            "project = \"TINES\"\nworkflow = \"IMPLEMENTATION\"\nstate = \"IMPLEMENT\"\nwrapper = [\"combined-wrapper\"]",
            "combined-wrapper",
        ),
    ];

    for (override_body, expected_wrapper) in cases {
        let config = config(&format!("[[override]]\n{override_body}"));
        let resolved = resolve_assignment(&config, &client, &assignment)
            .expect("resolve assignment configuration");

        assert_eq!(resolved.context().project(), "Tines");
        assert_eq!(resolved.context().workflow(), "Implementation");
        assert_eq!(resolved.context().state(), "Implement");
        assert_eq!(resolved.resolution().config.wrapper, [expected_wrapper]);
        assert_eq!(resolved.resolution().matching_overrides(), [0]);
    }

    for request in server.join().expect("join mock server") {
        let headers = request
            .split_once("\r\n\r\n")
            .expect("request headers")
            .0
            .to_ascii_lowercase();
        assert!(headers.starts_with("get /api/v1/issues/iss_assignment_test http/1.1"));
        assert!(headers.contains("authorization: bearer ephemeral-run-key"));
    }
}

#[test]
fn missing_project_metadata_fails_before_issue_lookup() {
    let mut without_project = assignment();
    without_project.run.issue_ref = None;
    let client = Client::with_timeout("http://127.0.0.1:1", Duration::from_millis(100))
        .expect("create issue client");
    let error = resolve_assignment(&config(""), &client, &without_project)
        .expect_err("missing project must fail resolution");

    assert!(error.to_string().contains("issue_ref.project_name"));
    assert!(!error.to_string().contains("ephemeral-run-key"));

    let mut without_state = assignment();
    without_state.run.state_at_start_name = None;
    let error = resolve_assignment(&config(""), &client, &without_state)
        .expect_err("missing state must fail resolution");
    assert!(error.to_string().contains("state_at_start_name"));
    assert!(!error.to_string().contains("ephemeral-run-key"));
}

#[test]
fn missing_or_unavailable_workflow_metadata_fails_clearly() {
    let (server_url, server) =
        issue_server_with_response(1, 200, r#"{"id":"iss_assignment_test"}"#);
    let client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let error = resolve_assignment(&config(""), &client, &assignment())
        .expect_err("missing workflow must fail resolution");
    assert!(
        error
            .to_string()
            .contains("did not include a workflow name")
    );
    let request = server.join().expect("join missing-workflow server");
    assert!(
        request[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer ephemeral-run-key")
    );

    let (server_url, server) = issue_server_with_response(
        1,
        404,
        r#"{"error":{"code":"not_found","message":"missing"}}"#,
    );
    let client =
        Client::with_timeout(&server_url, Duration::from_secs(5)).expect("create issue client");
    let error = resolve_assignment(&config(""), &client, &assignment())
        .expect_err("issue lookup failure must fail resolution");
    assert!(error.to_string().contains("could not fetch issue detail"));
    assert!(!error.to_string().contains("ephemeral-run-key"));

    let request = server.join().expect("join issue-error server");
    assert!(
        request[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer ephemeral-run-key")
    );
}
