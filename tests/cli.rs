use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tines-runner-cli-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn config_dir(&self) -> PathBuf {
        self.0.join("config")
    }

    fn credentials_path(&self) -> PathBuf {
        self.0.join("credentials.toml")
    }

    fn configure_command(&self, command: &mut Command) {
        let config_dir = self.config_dir();
        command
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir)
            .env("LOCALAPPDATA", &config_dir);
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read HTTP request");
        assert_ne!(count, 0, "client closed before sending the request");
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
            return String::from_utf8(request).expect("HTTP request should be UTF-8");
        }
    }
}

fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        responses
            .into_iter()
            .map(|(status, body)| {
                let (mut stream, _) = listener.accept().expect("accept Tines request");
                let request = read_http_request(&mut stream);
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    401 => "Unauthorized",
                    _ => "Mock",
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write mock response");
                request
            })
            .collect()
    });
    (format!("http://{address}"), server)
}

fn registration_server() -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        (0..2)
            .map(|_| {
                let (mut stream, _) = listener.accept().expect("accept Tines request");
                let request = read_http_request(&mut stream);
                let (status, body) = if request.contains("/runners/register ") {
                    (
                        201,
                        r#"{"runner":{"id":"rnr_cli_test"},"runner_token":"cli-runner-token"}"#,
                    )
                } else if request.contains("/runners/rnr_cli_test/poll ") {
                    let (_, body) = request
                        .split_once("\r\n\r\n")
                        .expect("runner poll request headers");
                    if body == "{" {
                        // The API authenticates first, then rejects malformed JSON before poll logic.
                        (400, r#"{"error":{"code":"invalid_json","message":"Request body must be valid JSON"}}"#)
                    } else {
                        // A valid poll would claim this one-shot assignment.
                        (
                            200,
                            r#"{"assignments":[{"run":{"id":"arun_queued","issue_id":"iss_queued"},"prompt":"queued work","bundle":{},"run_key":"run-key","timeout_minutes":30}],"cancels":[]}"#,
                        )
                    }
                } else {
                    (500, r#"{"error":"unexpected_request"}"#)
                };
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    400 => "Bad Request",
                    _ => "Mock",
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write mock response");
                request
            })
            .collect()
    });
    (format!("http://{address}"), server)
}

#[test]
fn version_flag_reports_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
        .arg("--version")
        .output()
        .expect("runner binary should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("tines-runner-rs {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn startup_registers_persists_credentials_and_restarts_without_bootstrap_key() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    let (server_url, server) = registration_server();
    let credentials_path =
        toml::Value::String(directory.credentials_path().to_string_lossy().into_owned());
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut first_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut first_start);
    let first_start = first_start
        .env("TINES_API_KEY", "bootstrap-key-test")
        .output()
        .expect("start runner for registration");
    let requests = server.join().expect("registration and startup token check");

    assert!(
        first_start.status.success(),
        "startup should register the runner: {}",
        String::from_utf8_lossy(&first_start.stderr)
    );
    let (headers, body) = requests[0]
        .split_once("\r\n\r\n")
        .expect("registration request headers");
    assert!(headers.contains("POST /api/v1/runners/register HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer bootstrap-key-test")
    );
    let body: serde_json::Value = serde_json::from_str(body).expect("registration request JSON");
    assert_eq!(body["name"], "cli-test-runner");
    assert_eq!(body["harness"], "codex");
    assert_eq!(body["max_concurrent"], 2);

    assert_eq!(
        requests.len(),
        2,
        "startup should register and validate once"
    );
    let (check_headers, check_body) = requests[1]
        .split_once("\r\n\r\n")
        .expect("startup token-check request headers");
    assert!(check_headers.contains("POST /api/v1/runners/rnr_cli_test/poll HTTP/1.1"));
    assert!(
        check_headers
            .to_ascii_lowercase()
            .contains("authorization: bearer cli-runner-token")
    );
    assert_eq!(check_body, "{", "startup must not send a valid poll body");

    let saved_credentials = fs::read_to_string(directory.credentials_path())
        .expect("read persisted runner credentials");
    assert!(saved_credentials.contains("rnr_cli_test"));
    assert!(saved_credentials.contains("cli-runner-token"));
    assert!(!saved_credentials.contains("bootstrap-key-test"));

    let (server_url, server) = mock_server(vec![(
        400,
        r#"{"error":{"code":"invalid_json","message":"Request body must be valid JSON"}}"#,
    )]);
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("update runner config for restart");
    let mut second_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut second_start);
    let second_start = second_start
        .env_remove("TINES_API_KEY")
        .output()
        .expect("restart runner from persisted credentials");
    assert!(
        second_start.status.success(),
        "restart should use saved credentials without TINES_API_KEY: {}",
        String::from_utf8_lossy(&second_start.stderr)
    );
    let request = server
        .join()
        .expect("restart token-check request")
        .remove(0);
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("restart token-check request headers");
    assert!(headers.contains("POST /api/v1/runners/rnr_cli_test/poll HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer cli-runner-token")
    );
    assert_eq!(body, "{");
}

#[test]
fn startup_fails_when_saved_runner_token_is_rejected() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    fs::write(
        directory.credentials_path(),
        "runner_id = \"rnr_rejected\"\nrunner_token = \"rejected-token\"\n",
    )
    .expect("write stored runner credentials");
    let (server_url, server) = mock_server(vec![(
        401,
        r#"{"error":{"code":"runner_token_invalid","message":"Invalid token"}}"#,
    )]);
    let credentials_path =
        toml::Value::String(directory.credentials_path().to_string_lossy().into_owned());
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut command);
    let output = command
        .env("TINES_API_KEY", "must-not-be-used")
        .output()
        .expect("start runner with a rejected token");

    assert!(!output.status.success(), "rejected token must fail startup");
    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostic.contains(
        "Tines rejected the runner token for runner rnr_rejected; stopped without falling back to TINES_API_KEY."
    ), "unexpected startup diagnostic: {diagnostic}");
    let request = server
        .join()
        .expect("rejected-token token-check request")
        .remove(0);
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("rejected-token token-check request headers");
    assert!(headers.contains("POST /api/v1/runners/rnr_rejected/poll HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer rejected-token")
    );
    assert_eq!(body, "{");
    assert!(!headers.contains("/register"));
}

#[test]
fn startup_fails_when_tines_is_unavailable() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    fs::write(
        directory.credentials_path(),
        "runner_id = \"rnr_offline\"\nrunner_token = \"saved-token\"\n",
    )
    .expect("write stored runner credentials");
    let unavailable = TcpListener::bind("127.0.0.1:0").expect("reserve local port");
    let address = unavailable.local_addr().expect("read local port");
    drop(unavailable);
    let credentials_path =
        toml::Value::String(directory.credentials_path().to_string_lossy().into_owned());
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = \"http://{address}\"\n[runner]\nname = \"cli-test-runner\"\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut command);
    let output = command
        .env_remove("TINES_API_KEY")
        .output()
        .expect("start runner while Tines is unavailable");

    assert!(
        !output.status.success(),
        "unavailable Tines must fail startup"
    );
    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("Tines runner protocol request failed"),
        "unexpected startup diagnostic: {diagnostic}"
    );
    assert!(
        diagnostic.contains("retryable transport/server"),
        "unexpected startup diagnostic: {diagnostic}"
    );
}
