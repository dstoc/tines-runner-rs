use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tines_runner_rs::assignment::resolve_assignment;
use tines_runner_rs::config::Config;
use tines_runner_rs::protocol::RunnerAssignment;
use tines_runner_rs::protocol::client::Client;

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
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
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
