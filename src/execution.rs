//! Run prepared assignments and settle their outcomes with Tines.

use std::error::Error;
use std::fmt;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::assignment::PreparedAssignment;
use crate::cancellation::CancellationToken;
use crate::codex::CodexLaunch;
use crate::codex_stream::CodexStreamParser;
use crate::config::WorkspaceRetention;
use crate::effort::EffortCapabilities;
use crate::finish::CodexRunReport;
use crate::process::{ProcessExit, ProcessOutput, SupervisedProcess};
use crate::protocol::client::{Client, ClientError, ErrorCategory};
use crate::protocol::{FinishRunRequest, FinishStatus};
use crate::retention::{self, RetentionError};
use crate::runner::{RunnerConnection, RunnerError};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Execute one assignment, report its outcome, then remove its workspace.
///
/// A retryable finish error keeps the workspace in place until Tines accepts
/// the report. A non-retryable error also leaves it in place for recovery.
pub fn execute_assignment(
    assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    capabilities: &EffortCapabilities,
    retention: &WorkspaceRetention,
) -> Result<(), ExecutionError> {
    execute_assignment_cancellable(
        assignment,
        connection,
        client,
        capabilities,
        retention,
        &CancellationToken::default(),
    )
    .map(|_| ())
}

/// Execute one assignment while honoring supervisor cancellation at every
/// process, log, and finish boundary.
pub fn execute_assignment_cancellable(
    mut assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    capabilities: &EffortCapabilities,
    retention: &WorkspaceRetention,
    cancellation: &CancellationToken,
) -> Result<ExecutionOutcome, ExecutionError> {
    if cancellation.is_cancelled() {
        assignment
            .workspace()
            .cleanup()
            .map_err(ExecutionError::WorkspaceCleanup)?;
        return Ok(ExecutionOutcome::Cancelled);
    }
    let run_id = assignment.assignment().run.id.clone();
    let runner_token = connection.credentials().runner_token();
    let secrets = assignment
        .workspace()
        .environment()
        .secret_values()
        .filter(|secret| !secret.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut report = CodexRunReport::new(assignment.assignment().run.model.as_deref());
    let timeout = Duration::from_secs(assignment.assignment().timeout_minutes.saturating_mul(60));
    let run_output = run_harness(
        &mut assignment,
        client,
        runner_token,
        capabilities,
        timeout,
        cancellation,
    );
    if cancellation.is_cancelled() || run_output.as_ref().is_ok_and(|output| output.cancelled) {
        assignment
            .workspace()
            .cleanup()
            .map_err(ExecutionError::WorkspaceCleanup)?;
        return Ok(ExecutionOutcome::Cancelled);
    }

    let (mut status, mut error, output) = match run_output {
        Ok(output) if !output.timed_out && output.exit == ProcessExit::Code(0) => {
            (FinishStatus::Completed, None, Some(output))
        }
        Ok(output) => {
            let error = process_error(&output, assignment.assignment().timeout_minutes);
            (FinishStatus::Failed, Some(error), Some(output))
        }
        Err(error) => (FinishStatus::Failed, Some(error), None),
    };
    if let Some(output) = output {
        collect_output(
            &mut report,
            &mut assignment,
            client,
            runner_token,
            &secrets,
            &output,
            cancellation,
        );
    }

    if cancellation.is_cancelled() {
        assignment
            .workspace()
            .cleanup()
            .map_err(ExecutionError::WorkspaceCleanup)?;
        return Ok(ExecutionOutcome::Cancelled);
    }

    if report.is_rate_limited() {
        status = FinishStatus::Failed;
        if error.is_none() {
            error = Some(
                report
                    .rate_limit_message()
                    .map(|message| format!("Codex provider usage limit: {message}"))
                    .unwrap_or_else(|| "Codex provider usage limit reached".to_owned()),
            );
        }
    }
    let error = error.map(|error| redact(&error, &secrets));

    let finish_request = report.into_finish_request(status, error);
    if !finish_with_retry(connection, &run_id, &finish_request, cancellation)? {
        assignment
            .workspace()
            .cleanup()
            .map_err(ExecutionError::WorkspaceCleanup)?;
        return Ok(ExecutionOutcome::Cancelled);
    }
    let issue_ref = assignment
        .assignment()
        .run
        .issue_ref
        .as_ref()
        .map(|issue| format!("{}/{}", issue.project_name, issue.number));
    retention::settle_workspace(
        assignment.workspace().path(),
        retention,
        &run_id,
        issue_ref,
        finish_request.status,
        finish_request.error.as_deref(),
    )
    .map_err(ExecutionError::WorkspaceRetention)?;
    Ok(ExecutionOutcome::Finished)
}

/// The terminal action taken for one assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    Finished,
    Cancelled,
}

/// Report a workspace-preparation failure unless supervisor cancellation has
/// already settled the run.
pub fn report_preparation_failure(
    connection: &RunnerConnection,
    run_id: &str,
    error: &str,
    cancellation: &CancellationToken,
) -> Result<ExecutionOutcome, ExecutionError> {
    let request = FinishRunRequest {
        status: FinishStatus::Failed,
        error: Some(error.to_owned()),
        provider_session_id: None,
        usage: None,
        pricing_evidence: None,
    };
    if finish_with_retry(connection, run_id, &request, cancellation)? {
        Ok(ExecutionOutcome::Finished)
    } else {
        Ok(ExecutionOutcome::Cancelled)
    }
}

fn run_harness(
    assignment: &mut PreparedAssignment,
    client: &Client,
    runner_token: &str,
    capabilities: &EffortCapabilities,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, String> {
    if cancellation.is_cancelled() {
        return Err("assignment was canceled before process launch".to_owned());
    }
    let launch = CodexLaunch::for_assignment(assignment, capabilities)
        .map_err(|error| format!("could not prepare Codex command: {error}"))?;
    tracing::info!(
        run_id = %assignment.assignment().run.id,
        launch = %launch,
        "starting Codex harness"
    );
    let mut command = launch.command();
    if cancellation.is_cancelled() {
        return Err("assignment was canceled before process launch".to_owned());
    }
    let process = SupervisedProcess::spawn(&mut command)
        .map_err(|error| format!("could not start Codex harness: {error}"))?;
    let deadline = Instant::now() + timeout;
    let mut run_logs = assignment.take_run_log_buffer();
    let log_client = client.clone();
    let log_run_id = assignment.assignment().run.id.clone();
    let log_token = runner_token.to_owned();
    let log_cancellation = cancellation.clone();
    let log_task = thread::spawn(move || {
        if !log_cancellation.is_cancelled()
            && let Some(Err(error)) = retry_protocol_until(deadline, &log_cancellation, || {
                run_logs.harness_started_until(&log_client, &log_run_id, &log_token, deadline)
            })
        {
            tracing::warn!(
                run_id = log_run_id,
                error = %error,
                "could not report that the Codex harness started"
            );
        }
        run_logs
    });

    let output = process
        .wait_timeout_or_cancel(
            deadline.saturating_duration_since(Instant::now()),
            TERMINATION_GRACE,
            cancellation,
        )
        .map_err(|error| format!("could not wait for Codex harness: {error}"))?;
    if output.cancelled || cancellation.is_cancelled() {
        // The start-log request is already in flight, if one started. It owns
        // no workspace state and checks cancellation before retrying.
        drop(log_task);
        return Ok(output);
    }
    assignment.restore_run_log_buffer(
        log_task
            .join()
            .map_err(|_| "harness-start log worker panicked".to_owned())?,
    );
    Ok(output)
}

fn collect_output(
    report: &mut CodexRunReport,
    assignment: &mut PreparedAssignment,
    client: &Client,
    runner_token: &str,
    secrets: &[String],
    output: &ProcessOutput,
    cancellation: &CancellationToken,
) {
    if cancellation.is_cancelled() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parser = CodexStreamParser::default();
    let events = parser
        .push(&stdout)
        .into_iter()
        .chain(parser.finish())
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    for event in events {
        report.observe(&event);
        lines.extend(event.render_lines());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        lines.extend(stderr.lines().map(|line| format!("[stderr] {line}")));
    }

    let rendered = lines
        .into_iter()
        .map(|line| redact(&line, secrets))
        .collect::<Vec<_>>()
        .join("\n");
    if rendered.is_empty() {
        return;
    }

    if let Err(error) = retry_protocol(cancellation, || {
        assignment.append_harness_output(client, runner_token, &format!("{rendered}\n"))
    }) {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not append Codex output to the Tines run log"
        );
    }
}

fn process_error(output: &ProcessOutput, timeout_minutes: u64) -> String {
    let reason = if output.timed_out {
        format!("Codex exceeded the {timeout_minutes}-minute run timeout")
    } else {
        match output.exit {
            ProcessExit::Code(code) => format!("Codex exited with code {code}"),
            ProcessExit::Signal(signal) => format!("Codex was terminated by signal {signal}"),
            ProcessExit::Unknown => "Codex exited with an unknown status".to_owned(),
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        reason
    } else {
        format!("{reason}\nCodex stderr:\n{}", stderr.trim())
    }
}

fn redact(value: &str, secrets: &[String]) -> String {
    secrets.iter().fold(value.to_owned(), |output, secret| {
        output.replace(secret, "[REDACTED]")
    })
}

fn finish_with_retry(
    connection: &RunnerConnection,
    run_id: &str,
    request: &FinishRunRequest,
    cancellation: &CancellationToken,
) -> Result<bool, ExecutionError> {
    let mut failures = 0u32;
    loop {
        if cancellation.is_cancelled() {
            return Ok(false);
        }
        match connection.finish_assignment(run_id, request) {
            Ok(_) => return Ok(true),
            Err(RunnerError::Protocol(error)) if error.category() == ErrorCategory::Retryable => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                tracing::warn!(
                    run_id,
                    error = %error,
                    backoff_seconds = delay.as_secs(),
                    "finish report failed; retrying"
                );
                cancellation.wait(delay);
            }
            Err(error) => return Err(ExecutionError::FinishReport(error)),
        }
    }
}

fn retry_protocol<T>(
    cancellation: &CancellationToken,
    mut request: impl FnMut() -> Result<T, ClientError>,
) -> Result<(), ClientError> {
    let mut failures = 0u32;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match request() {
            Ok(_) => return Ok(()),
            Err(error) if error.category() == ErrorCategory::Retryable => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                cancellation.wait(delay);
            }
            Err(error) => return Err(error),
        }
    }
}

fn retry_protocol_until<T>(
    deadline: Instant,
    cancellation: &CancellationToken,
    mut request: impl FnMut() -> Result<T, ClientError>,
) -> Option<Result<(), ClientError>> {
    let mut failures = 0u32;
    loop {
        if cancellation.is_cancelled() {
            return None;
        }
        match request() {
            Ok(_) => return Some(Ok(())),
            Err(error) if error.category() == ErrorCategory::Retryable => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Some(Err(error));
                }
                let delay = retry_delay(failures).min(remaining);
                failures = failures.saturating_add(1);
                tracing::warn!(
                    error = %error,
                    backoff_seconds = delay.as_secs_f64(),
                    "harness-start log request failed; retrying before the run deadline"
                );
                cancellation.wait(delay);
            }
            Err(error) => return Some(Err(error)),
        }
    }
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(6)).min(MAX_BACKOFF)
}

/// An error that prevented local settlement of a completed harness attempt.
#[derive(Debug)]
pub enum ExecutionError {
    FinishReport(RunnerError),
    WorkspaceCleanup(io::Error),
    WorkspaceRetention(RetentionError),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FinishReport(error) => write!(f, "could not report run finish: {error}"),
            Self::WorkspaceCleanup(error) => {
                write!(
                    f,
                    "run finish was accepted, but workspace cleanup failed: {error}"
                )
            }
            Self::WorkspaceRetention(error) => write!(
                f,
                "run finish was accepted, but workspace retention failed: {error}"
            ),
        }
    }
}

impl Error for ExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FinishReport(error) => Some(error),
            Self::WorkspaceCleanup(error) => Some(error),
            Self::WorkspaceRetention(error) => Some(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::run_harness;
    use crate::assignment::{PreparedAssignment, resolve_assignment};
    use crate::cancellation::CancellationToken;
    use crate::config::Config;
    use crate::effort::EffortCapabilities;
    use crate::protocol::RunnerAssignment;
    use crate::protocol::client::Client;
    use crate::workspace::MaterializedWorkspace;
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tines-runner-execution-timeout-{}-{id}",
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
            let count = stream.read(&mut chunk).expect("read fake Tines request");
            assert_ne!(count, 0, "client closed before completing its request");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
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

    fn respond(stream: &mut TcpStream, status: u16, body: &str) {
        let label = if status == 200 { "OK" } else { "Unavailable" };
        write!(
            stream,
            "HTTP/1.1 {status} {label}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write fake Tines response");
    }

    #[test]
    fn retrying_harness_start_log_does_not_extend_the_process_timeout() {
        let directory = TestDirectory::new();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
        let address = listener.local_addr().expect("read fake Tines address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept fake Tines request");
                let request = read_request(&mut stream);
                if request.starts_with("GET /api/v1/issues/iss_timeout ") {
                    respond(
                        &mut stream,
                        200,
                        r#"{"id":"iss_timeout","workflow":{"name":"Implementation"}}"#,
                    );
                } else if request.starts_with("POST /api/v1/runs/arun_timeout/logs ") {
                    respond(
                        &mut stream,
                        503,
                        r#"{"error":{"code":"unavailable","message":"retry"}}"#,
                    );
                } else {
                    panic!("unexpected fake Tines request: {request}");
                }
                requests.push(request);
            }
            requests
        });
        let server_url = format!("http://{address}");
        let wrapper_path = directory.0.join("slow-codex-wrapper");
        fs::write(&wrapper_path, "#!/bin/sh\nexec sleep 30\n").expect("write wrapper");
        fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))
            .expect("make wrapper executable");
        let workspace_parent = directory.0.join("workspaces");
        let config = Config::from_toml_str(&format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"timeout-test\"\nwrapper = [{}]\nworkspace_parent = {:?}\n",
            serde_json::to_string(&wrapper_path.to_string_lossy().as_ref())
                .expect("encode wrapper path"),
            workspace_parent
        ))
        .expect("parse runner config");
        let client = Client::with_timeout(&server_url, Duration::from_secs(5))
            .expect("create protocol client");
        let protocol_assignment: RunnerAssignment = serde_json::from_value(json!({
            "run": {
                "id": "arun_timeout",
                "issue_id": "iss_timeout",
                "issue_ref": {"project_name": "Tines", "number": 14, "title": "Timeout"},
                "state_at_start_name": "Implement"
            },
            "prompt": "work",
            "bundle": {"skills": [], "repos": []},
            "run_key": "issue-key",
            "timeout_minutes": 1,
            "env": []
        }))
        .expect("decode assignment");
        let resolved =
            resolve_assignment(&config, &client, &protocol_assignment).expect("resolve assignment");
        let workspace = MaterializedWorkspace::create(
            &resolved.resolution().config.workspace_parent,
            resolved.assignment(),
            &config.server_url,
        )
        .expect("create assignment workspace");
        let mut prepared = PreparedAssignment::new(resolved, workspace);
        let capabilities = EffortCapabilities {
            version: 1,
            daemon_version: "test-runner".to_owned(),
            harness: "codex".to_owned(),
            harness_version: "stub".to_owned(),
            catalog_digest: "empty".to_owned(),
            models: Vec::new(),
            accepts_asserted_effort: Some(false),
            discovery_error: None,
        };

        let started = Instant::now();
        let output = run_harness(
            &mut prepared,
            &client,
            "runner-token",
            &capabilities,
            Duration::from_millis(200),
            &CancellationToken::default(),
        )
        .expect("supervise harness after start-log outage");

        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
        let requests = server.join().expect("join fake Tines server");
        assert!(requests[0].starts_with("GET /api/v1/issues/iss_timeout "));
        assert!(requests[1].starts_with("POST /api/v1/runs/arun_timeout/logs "));
    }
}
