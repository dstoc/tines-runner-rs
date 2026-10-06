use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
#[cfg(unix)]
use std::time::{Duration, Instant};
use tines_runner_rs::execution_protocol::{
    ExecutionEventKind, ExecutionEventParser, TerminalStatus,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

#[cfg(unix)]
struct RunnerGuard {
    child: Child,
    credentials_path: PathBuf,
}

#[cfg(unix)]
impl Drop for RunnerGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        if let Ok(store) = tines_runner_rs::recovery::ActiveRunStore::open(
            self.credentials_path.with_file_name("active-runs.json"),
        ) {
            for record in store.records() {
                if let Some(process) = record.transport {
                    let _ = process.terminate_if_matches(Duration::from_millis(100));
                }
            }
        }
    }
}

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

    fn runner_config_path(&self) -> PathBuf {
        self.config_dir()
            .join("tines-runner-rs")
            .join("config.toml")
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

    fn select_runner_config(&self, command: &mut Command) {
        command.arg("--config").arg(self.runner_config_path());
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_executor(input: &[u8]) -> std::process::Output {
    run_executor_with_path(input, None)
}

fn run_executor_with_path(input: &[u8], path: Option<std::ffi::OsString>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    command
        .arg("execute")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().expect("start executor CLI");
    child
        .stdin
        .take()
        .expect("executor stdin")
        .write_all(input)
        .expect("write executor request");
    child.wait_with_output().expect("wait for executor CLI")
}

#[cfg(unix)]
#[test]
fn capabilities_mode_reports_codex_from_its_executor_environment() {
    let directory = TestDirectory::new();
    let bin = directory.0.join("bin");
    fs::create_dir_all(&bin).expect("create fake executor PATH");
    let codex = bin.join("codex");
    fs::write(&codex, include_str!("support/stub_codex.sh")).expect("write Codex stub");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
        .expect("make Codex stub executable");
    let mut search_path = vec![bin];
    search_path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let search_path = std::env::join_paths(search_path).expect("build fake executor PATH");

    let output = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
        .arg("capabilities")
        .env("PATH", search_path)
        .output()
        .expect("run executor capability mode");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("decode capability document");
    assert_eq!(document["version"], 1);
    assert_eq!(
        document["harnesses"]["codex"]["version"],
        "codex-fake 0.1.0"
    );
    assert_eq!(
        document["harnesses"]["codex"]["effort"]["models"],
        serde_json::json!([])
    );
}

#[cfg(unix)]
fn run_executor_with_codex(
    input: &[u8],
    directory: &TestDirectory,
    script: &str,
) -> std::process::Output {
    let bin_directory = directory.0.join("executor-bin");
    fs::create_dir_all(&bin_directory).expect("create executor bin directory");
    let codex = bin_directory.join("codex");
    fs::write(&codex, script).expect("write Codex stub");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
        .expect("make Codex stub executable");
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_directory).chain(std::env::split_paths(&current_path)),
    )
    .expect("compose executor PATH");
    run_executor_with_path(input, Some(path))
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

#[cfg(unix)]
fn shutdown_server() -> (String, JoinHandle<(Vec<String>, serde_json::Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut requests = Vec::new();
        let mut assignment_sent = false;
        let mut finish = None;
        let mut draining_seen = false;
        loop {
            assert!(Instant::now() < deadline, "shutdown server timed out");
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept Tines request: {error}"),
            };
            let request = read_http_request(&mut stream);
            let (headers, body) = request
                .split_once("\r\n\r\n")
                .expect("Tines request headers");
            let mut done = false;
            let response = if headers.starts_with("POST /api/v1/runners/rnr_shutdown/poll ") {
                let poll: serde_json::Value = serde_json::from_str(body).expect("poll JSON");
                draining_seen |= poll["draining"] == true;
                if !assignment_sent {
                    assignment_sent = true;
                    r#"{"assignments":[{"run":{"id":"arun_shutdown","issue_id":"iss_shutdown","issue_ref":{"project_name":"Tines","number":18,"title":"Graceful shutdown"},"state_at_start_name":"Implement"},"prompt":"wait for shutdown","bundle":{"skills":[],"repos":[]},"run_key":"issue-run-key","timeout_minutes":5}],"cancels":[]}"#.to_owned()
                } else {
                    r#"{"assignments":[],"cancels":[]}"#.to_owned()
                }
            } else if headers.starts_with("GET /api/v1/issues/iss_shutdown ") {
                r#"{"id":"iss_shutdown","workflow":{"name":"Implementation"}}"#.to_owned()
            } else if headers.starts_with("POST /api/v1/runs/arun_shutdown/logs ") {
                let log: serde_json::Value = serde_json::from_str(body).expect("log JSON");
                let sequence = log["seq"].as_u64().expect("run-log sequence");
                format!(r#"{{"status":"running","log_bytes_dropped":0,"log_seq":{sequence}}}"#)
            } else if headers.starts_with("POST /api/v1/runs/arun_shutdown/finish ") {
                finish = Some(serde_json::from_str(body).expect("finish JSON"));
                r#"{"id":"arun_shutdown","status":"failed"}"#.to_owned()
            } else {
                panic!("unexpected Tines request: {request}");
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .expect("write Tines response");
            requests.push(request);
            done |= finish.is_some() && draining_seen;
            if done {
                break;
            }
        }
        (requests, finish.expect("run finish report"))
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
fn daemon_and_check_require_an_explicit_config_path() {
    let directory = TestDirectory::new();
    let config_path = directory.runner_config_path();
    fs::create_dir_all(config_path.parent().unwrap()).expect("create default config directory");
    fs::write(&config_path, "[server\nurl = [broken").expect("write implicit config fixture");

    for check in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
        if check {
            command.arg("--check");
        }
        directory.configure_command(&mut command);
        let output = command.output().expect("run without --config");
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(!output.status.success(), "missing --config must fail");
        assert!(
            diagnostic.contains("--config <PATH> is required"),
            "missing --config should produce a usage error: {diagnostic}"
        );
        assert!(
            diagnostic.contains("Usage:"),
            "missing --config should show usage: {diagnostic}"
        );
        assert!(
            !diagnostic.contains("invalid config file"),
            "the XDG config location must not be probed: {diagnostic}"
        );
    }
}

#[cfg(unix)]
#[test]
fn execute_runs_harness_and_emits_one_terminal_protocol_result() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("executor-workspaces");
    let mut fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/execution-request-v1.json"))
            .expect("valid execution request fixture");
    fixture["execution"]["workspace"]["parent"] =
        serde_json::Value::String(workspace_parent.to_string_lossy().into_owned());
    fixture["execution"]["retention"]["mode"] = serde_json::Value::String("never".to_owned());
    fixture["assignment"]["effort"] = serde_json::Value::Null;
    let output = run_executor_with_codex(
        fixture.to_string().as_bytes(),
        &directory,
        "#!/bin/sh\nexit 0\n",
    );

    assert!(
        output.status.success(),
        "execute rejected a valid request: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let mut parser = ExecutionEventParser::default();
    let events = parser
        .push(&output.stdout)
        .expect("executor output should be JSONL");
    assert!(parser.finish().unwrap().is_none());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, ExecutionEventKind::Result { .. }))
            .count(),
        1
    );
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(ExecutionEventKind::Result { result }) if result.status == TerminalStatus::Completed
    ));
    assert!(
        fs::read_dir(workspace_parent)
            .expect("executor creates its selected workspace parent")
            .next()
            .is_none(),
        "one-shot preparation removes its workspace after successful setup"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn execute_sigterm_stops_harness_descendants_and_cleans_workspace() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("executor-workspaces");
    let bin_directory = directory.0.join("executor-bin");
    fs::create_dir_all(&bin_directory).expect("create executor bin directory");
    let codex = bin_directory.join("codex");
    let harness_pid_path = directory.0.join("harness.pid");
    let descendant_pid_path = directory.0.join("descendant.pid");
    fs::write(
        &codex,
        format!(
            "#!/bin/sh\ntrap '' TERM\n(trap '' TERM; exec sleep 600) &\ndescendant=$!\nprintf '%s\\n' \"$$\" > '{}.tmp'\nmv '{}.tmp' '{}'\nprintf '%s\\n' \"$descendant\" > '{}.tmp'\nmv '{}.tmp' '{}'\nwait \"$descendant\"\n",
            harness_pid_path.display(),
            harness_pid_path.display(),
            harness_pid_path.display(),
            descendant_pid_path.display(),
            descendant_pid_path.display(),
            descendant_pid_path.display(),
        ),
    )
    .expect("write Codex stub");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
        .expect("make Codex stub executable");

    let mut fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/execution-request-v1.json"))
            .expect("valid execution request fixture");
    fixture["execution"]["workspace"]["parent"] =
        serde_json::Value::String(workspace_parent.to_string_lossy().into_owned());
    fixture["execution"]["retention"]["mode"] = serde_json::Value::String("always".to_owned());
    fixture["assignment"]["effort"] = serde_json::Value::Null;

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
    command.process_group(0);
    let mut executor = command.spawn().expect("start executor CLI");
    executor
        .stdin
        .take()
        .expect("executor stdin")
        .write_all(fixture.to_string().as_bytes())
        .expect("write executor request");

    let deadline = Instant::now() + Duration::from_secs(15);
    let harness_pid = wait_for_pid_file(&mut executor, &harness_pid_path, deadline);
    let descendant_pid = wait_for_pid_file(&mut executor, &descendant_pid_path, deadline);

    let signal = Command::new("kill")
        .args(["-TERM", "--", &format!("-{}", executor.id())])
        .status()
        .expect("send SIGTERM to executor process group");
    assert!(signal.success(), "SIGTERM command failed");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = executor.try_wait().expect("wait for executor shutdown") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "executor did not stop after SIGTERM"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success(), "interrupted executor reported success");
    assert_process_stopped(harness_pid);
    assert_process_stopped(descendant_pid);

    let output = executor
        .wait_with_output()
        .expect("collect executor output after shutdown");
    let stdout = String::from_utf8(output.stdout).expect("executor output is UTF-8");
    let results = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event.get("type").and_then(serde_json::Value::as_str) == Some("result"))
        .count();
    assert_eq!(
        results, 0,
        "supervisor cancellation emitted a terminal result"
    );
    assert!(
        fs::read_dir(&workspace_parent)
            .expect("read workspace parent after interruption")
            .next()
            .is_none(),
        "interruption left a workspace or retained-workspace marker"
    );
}

#[cfg(target_os = "linux")]
fn wait_for_pid_file(child: &mut Child, path: &std::path::Path, deadline: Instant) -> u32 {
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(process_id) = contents.trim().parse::<u32>()
        {
            return process_id;
        }
        if let Some(status) = child.try_wait().expect("check executor startup") {
            panic!("executor exited before harness startup: {status}");
        }
        assert!(Instant::now() < deadline, "harness did not publish its PID");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn execute_reports_workspace_failures_as_protocol_results_without_tines_calls() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("executor-workspaces");
    let mut fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/execution-request-v1.json"))
            .expect("valid execution request fixture");
    fixture["execution"]["workspace"]["parent"] =
        serde_json::Value::String(workspace_parent.to_string_lossy().into_owned());
    fixture["assignment"]["bundle"]["repos"] = serde_json::json!([{
        "name": "missing-repo",
        "dir": "checkout",
        "url": directory.0.join("missing-repository").to_string_lossy(),
        "branch": null
    }]);

    let output = run_executor(fixture.to_string().as_bytes());
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let mut parser = ExecutionEventParser::default();
    let events = parser
        .push(&output.stdout)
        .expect("executor failure should be JSONL");
    assert!(parser.finish().unwrap().is_none());
    let results = events
        .iter()
        .filter_map(|event| match &event.kind {
            ExecutionEventKind::Result { result } => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    let result = results[0];
    assert_eq!(result.status, TerminalStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .contains("git clone failed")
    );
    let line = String::from_utf8(output.stdout).expect("protocol result is UTF-8");
    assert!(!line.contains("fixture-run-key"));
    assert!(!line.contains("fixture-secret"));
    assert!(
        fs::read_dir(workspace_parent)
            .expect("read workspace parent")
            .next()
            .is_none(),
        "failed setup removes its incomplete workspace"
    );
}

#[test]
fn execute_redacts_escaped_secret_values_in_clone_failure_results() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("executor-workspaces");

    for secret in ["private\"value", "private\nvalue"] {
        let mut fixture: serde_json::Value =
            serde_json::from_slice(include_bytes!("fixtures/execution-request-v1.json"))
                .expect("valid execution request fixture");
        fixture["execution"]["workspace"]["parent"] =
            serde_json::Value::String(workspace_parent.to_string_lossy().into_owned());
        fixture["assignment"]["env"][0]["value"] = serde_json::Value::String(secret.to_owned());
        fixture["assignment"]["bundle"]["repos"] = serde_json::json!([{
            "name": "missing-repo",
            "dir": secret,
            "url": directory.0.join("missing-repository").to_string_lossy(),
            "branch": null
        }]);

        let output = run_executor(fixture.to_string().as_bytes());
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let mut parser = ExecutionEventParser::default();
        let events = parser
            .push(&output.stdout)
            .expect("executor failure should be JSONL");
        assert!(parser.finish().unwrap().is_none());
        let results = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Result { result } => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1);
        let result = results[0];
        let error = result.error.as_deref().expect("failure result has error");
        let rust_escaped = format!("{secret:?}");
        let rust_escaped = &rust_escaped[1..rust_escaped.len() - 1];

        assert!(error.contains("git clone failed"));
        assert!(error.contains("***"));
        assert!(!error.contains(secret), "raw secret leaked for {secret:?}");
        assert!(
            !error.contains(rust_escaped),
            "escaped secret leaked for {secret:?}"
        );
        let line = String::from_utf8(output.stdout).expect("protocol result is UTF-8");
        assert!(!line.contains(rust_escaped));
        assert!(
            fs::read_dir(&workspace_parent)
                .expect("read workspace parent")
                .next()
                .is_none(),
            "failed setup removes its incomplete workspace"
        );
    }
}

#[test]
fn execute_rejects_bad_requests_without_echoing_secret_content() {
    let malformed =
        run_executor(br#"{"version":1,"assignment":{"run_key":"raw-run-key-secret",not-json}}"#);
    assert!(!malformed.status.success());
    assert!(malformed.stdout.is_empty());
    let malformed_diagnostic = String::from_utf8_lossy(&malformed.stderr);
    assert_eq!(
        malformed_diagnostic,
        "executor request rejected: malformed execution request JSON\n"
    );
    assert!(!malformed_diagnostic.contains("raw-run-key-secret"));

    let fixture = include_str!("fixtures/execution-request-v1.json");
    let unsupported = fixture.replace("\"version\": 1", "\"version\": 37");
    let unsupported = run_executor(unsupported.as_bytes());
    assert!(!unsupported.status.success());
    assert!(unsupported.stdout.is_empty());
    let unsupported_diagnostic = String::from_utf8_lossy(&unsupported.stderr);
    assert_eq!(
        unsupported_diagnostic,
        "executor request rejected: unsupported execution protocol version 37\n"
    );
    assert!(!unsupported_diagnostic.contains("fixture-run-key"));
    assert!(!unsupported_diagnostic.contains("fixture-secret"));
}

#[test]
fn explicit_configs_register_separate_runners_and_credentials() {
    let directory = TestDirectory::new();

    for (runner_name, config_name) in [("build-codex", "build"), ("review-codex", "review")] {
        let config_path = directory.0.join(format!("{config_name}.toml"));
        let credentials_path = directory.0.join(config_name).join("credentials.toml");
        let (server_url, server) = registration_server();
        let credentials_value =
            toml::Value::String(credentials_path.to_string_lossy().into_owned());
        fs::write(
            &config_path,
            format!(
                "[server]\nurl = {server_url:?}\n[runner]\nname = {runner_name:?}\nexecutor_cwd = \"~\"\n[storage]\ncredentials_file = {credentials_value}\n"
            ),
        )
        .expect("write selected runner config");

        let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
        command.args([
            "--config",
            config_path.to_str().expect("config path should be UTF-8"),
            "--check",
        ]);
        directory.configure_command(&mut command);
        let output = command
            .env("TINES_API_KEY", "bootstrap-key-test")
            .output()
            .expect("check selected runner config");
        let requests = server.join().expect("registration and token check");

        assert!(
            output.status.success(),
            "--check should use {}: {}",
            config_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let (_, body) = requests[0]
            .split_once("\r\n\r\n")
            .expect("registration request headers");
        let body: serde_json::Value = serde_json::from_str(body).expect("registration JSON");
        assert_eq!(body["name"], runner_name);
        assert!(
            credentials_path.is_file(),
            "selected config should save credentials to {}",
            credentials_path.display()
        );
    }
}

#[test]
fn selected_config_errors_name_the_file() {
    let directory = TestDirectory::new();
    let missing_path = directory.0.join("missing.toml");
    let mut missing_command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    missing_command.args([
        "--config",
        missing_path.to_str().expect("config path should be UTF-8"),
        "--check",
    ]);
    directory.configure_command(&mut missing_command);
    let missing_output = missing_command.output().expect("run with a missing config");
    let missing_diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&missing_output.stdout),
        String::from_utf8_lossy(&missing_output.stderr)
    );
    assert!(!missing_output.status.success());
    assert!(
        missing_diagnostic.contains(&format!(
            "could not read config file {}",
            missing_path.display()
        )),
        "unexpected missing-config diagnostic: {missing_diagnostic}"
    );

    let invalid_path = directory.0.join("invalid.toml");
    fs::write(&invalid_path, "[server\nurl = [broken").expect("write invalid TOML");
    let mut invalid_command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    invalid_command.args([
        "--config",
        invalid_path.to_str().expect("config path should be UTF-8"),
        "--check",
    ]);
    directory.configure_command(&mut invalid_command);
    let invalid_output = invalid_command.output().expect("run with invalid config");
    let invalid_diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&invalid_output.stdout),
        String::from_utf8_lossy(&invalid_output.stderr)
    );
    assert!(!invalid_output.status.success());
    assert!(
        invalid_diagnostic.contains(&format!("invalid config file {}", invalid_path.display())),
        "unexpected invalid-config diagnostic: {invalid_diagnostic}"
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
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nexecutor_cwd = \"~\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut first_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut first_start);
    first_start.arg("--check");
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
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nexecutor_cwd = \"~\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("update runner config for restart");
    let mut second_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut second_start);
    second_start.arg("--check");
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
fn daemon_uses_its_first_poll_to_authenticate_and_detect_fencing() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    fs::write(
        directory.credentials_path(),
        "runner_id = \"rnr_daemon\"\nrunner_token = \"daemon-token\"\n",
    )
    .expect("write stored runner credentials");
    let (server_url, server) = mock_server(vec![(
        409,
        r#"{"error":{"code":"runner_conflict","message":"another daemon instance is serving this runner; this one has been superseded"}}"#,
    )]);
    let credentials_path =
        toml::Value::String(directory.credentials_path().to_string_lossy().into_owned());
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nexecutor_cwd = \"~\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut command);
    directory.configure_command(&mut command);
    let output = command
        .env_remove("TINES_API_KEY")
        .output()
        .expect("start daemon with a superseded runner token");

    assert!(!output.status.success(), "superseded daemon must exit");
    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("Tines superseded this daemon for runner rnr_daemon; exiting"),
        "unexpected daemon diagnostic: {diagnostic}"
    );

    let request = server
        .join()
        .expect("daemon's first poll request")
        .remove(0);
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("poll request headers");
    assert!(headers.contains("POST /api/v1/runners/rnr_daemon/poll HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer daemon-token")
    );
    let body: serde_json::Value = serde_json::from_str(body).expect("first poll request JSON");
    assert!(body["instance_id"].as_str().is_some());
    assert_eq!(body["owned_runs"], serde_json::json!([]));
    assert_eq!(body["max_concurrent"], 2);
    assert_eq!(body["draining"], false);
    assert_eq!(body["env_delivery"], 1);
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
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nexecutor_cwd = \"~\"\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut command);
    command.arg("--check");
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
            "[server]\nurl = \"http://{address}\"\n[runner]\nname = \"cli-test-runner\"\nexecutor_cwd = \"~\"\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut command);
    command.arg("--check");
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

#[cfg(target_os = "linux")]
#[test]
fn sigterm_drains_daemon_kills_harness_and_reports_interrupted() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    let (server_url, server) = shutdown_server();
    let credentials_path = directory.credentials_path();
    fs::write(
        &credentials_path,
        "runner_id = \"rnr_shutdown\"\nrunner_token = \"shutdown-token\"\n",
    )
    .expect("write runner credentials");
    let executor_path = std::path::Path::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    let bin_directory = directory.0.join("bin");
    fs::create_dir_all(&bin_directory).expect("create executor PATH directory");
    let codex = bin_directory.join("codex");
    fs::write(&codex, include_str!("support/stub_codex.sh")).expect("write Codex stub");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
        .expect("make Codex stub executable");
    let harness_pid_path = directory.0.join("harness.pid");
    let descendant_path = directory.0.join("descendant.pid");
    let workspace_parent = directory.0.join("workspaces");
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_directory).chain(std::env::split_paths(&current_path)),
    )
    .expect("compose Codex PATH");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"shutdown-test\"\nexecutor_cwd = \"~\"\nexecutor = [{}]\nworkspace_parent = {:?}\nmax_concurrent = 1\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {:?}\n",
            serde_json::to_string(&executor_path.to_string_lossy().as_ref())
                .expect("encode executor path"),
            workspace_parent,
            credentials_path
        ),
    )
    .expect("write runner config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.select_runner_config(&mut command);
    directory.configure_command(&mut command);
    command
        .env("PATH", path)
        .env("FAKE_CODEX_HARNESS_PID_FILE", &harness_pid_path)
        .env("FAKE_CODEX_CHILD_PID_FILE", &descendant_path);
    let mut runner = RunnerGuard {
        child: command
            .env_remove("TINES_API_KEY")
            .spawn()
            .expect("start runner daemon"),
        credentials_path: credentials_path.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while (!harness_pid_path.exists() || !descendant_path.exists()) && Instant::now() < deadline {
        if let Some(status) = runner.child.try_wait().expect("check runner") {
            panic!("runner exited before starting harness: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(harness_pid_path.exists(), "Codex harness did not start");
    assert!(descendant_path.exists(), "harness descendant did not start");
    let harness = wait_for_pid_file(&mut runner.child, &harness_pid_path, deadline);
    let descendant = wait_for_pid_file(&mut runner.child, &descendant_path, deadline);
    let active_runs_path = credentials_path.with_file_name("active-runs.json");
    let transport_identity = loop {
        if let Some(status) = runner.child.try_wait().expect("check runner") {
            panic!("runner exited before executor cancellation: {status}");
        }
        let store = tines_runner_rs::recovery::ActiveRunStore::open(&active_runs_path)
            .expect("read active-run state");
        if let Some(identity) = store
            .records()
            .into_iter()
            .find(|record| record.run_id == "arun_shutdown")
            .and_then(|record| record.transport)
        {
            break identity;
        }
        assert!(
            Instant::now() < deadline,
            "transport identity was not persisted"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let signal = Command::new("kill")
        .args(["-TERM", &runner.child.id().to_string()])
        .status()
        .expect("send SIGTERM to runner");
    assert!(signal.success(), "SIGTERM command failed");
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = runner.child.try_wait().expect("wait for runner shutdown") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "runner did not drain and exit"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "graceful shutdown exited with {status:?}");
    assert_process_stopped(harness);
    assert_process_stopped(descendant);
    assert_process_stopped(transport_identity.process_id());
    assert!(
        !transport_identity.matches_live_process(),
        "the persisted executor transport identity must stop"
    );

    let (requests, finish) = server.join().expect("join shutdown server");
    assert!(
        requests.iter().any(|request| {
            let (_, body) = request.split_once("\r\n\r\n").unwrap_or_default();
            serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .is_some_and(|body| body["draining"] == true)
        }),
        "runner never reported draining"
    );
    assert_eq!(finish["status"], "failed");
    assert_eq!(finish["judgment"], "interrupted");
    assert!(
        finish["error"]
            .as_str()
            .unwrap_or_default()
            .contains("shutdown")
    );
    let active_runs = tines_runner_rs::recovery::ActiveRunStore::open(active_runs_path)
        .expect("read settled active-run state");
    assert!(active_runs.records().is_empty());
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
            Err(error) => panic!("could not inspect descendant: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "harness descendant remained alive"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
