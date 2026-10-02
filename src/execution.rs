//! Run prepared assignments and settle their outcomes with Tines.

use std::error::Error;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::assignment::PreparedAssignment;
use crate::cancellation::CancellationToken;
use crate::codex::CodexLaunch;
use crate::codex_stream::{CodexEvent, CodexStreamParser};
use crate::config::WorkspaceRetention;
use crate::effort::EffortCapabilities;
use crate::finish::CodexRunReport;
use crate::process::{ProcessExit, ProcessOutput, ProcessStream, SupervisedProcess};
use crate::protocol::client::{Client, ClientError, ErrorCategory};
use crate::protocol::{FinishRunRequest, FinishStatus};
use crate::retention::{self, RetentionError};
use crate::runner::{RunnerConnection, RunnerError};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const MAX_STDERR_DIAGNOSTIC_BYTES: usize = 32 * 1024;

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
        assignment.stop_log_delivery();
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
    let log_context = RunLogContext {
        client,
        runner_token,
        cancellation,
        secrets: &secrets,
    };
    let mut report = CodexRunReport::new(assignment.assignment().run.model.as_deref());
    let timeout = Duration::from_secs(assignment.assignment().timeout_minutes.saturating_mul(60));
    let run_output = run_harness(
        &mut assignment,
        &log_context,
        capabilities,
        timeout,
        &mut report,
    );

    if cancellation.is_cancelled()
        || assignment.log_delivery_cancelled()
        || run_output.as_ref().is_ok_and(|output| output.cancelled)
    {
        assignment.stop_log_delivery();
        assignment
            .workspace()
            .cleanup()
            .map_err(ExecutionError::WorkspaceCleanup)?;
        return Ok(ExecutionOutcome::Cancelled);
    }

    let (mut status, mut error) = match run_output {
        Ok(output) if !output.timed_out && output.exit == ProcessExit::Code(0) => {
            (FinishStatus::Completed, None)
        }
        Ok(output) => {
            let error = process_error(&output, assignment.assignment().timeout_minutes);
            (FinishStatus::Failed, Some(error))
        }
        Err(error) => (FinishStatus::Failed, Some(error)),
    };
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
    if !finish_with_retry(
        connection,
        &run_id,
        &assignment.run_log_buffer(),
        &finish_request,
        cancellation,
    )? {
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
    logs: &crate::protocol::client::RunLogBuffer,
    error: &str,
    cancellation: &CancellationToken,
) -> Result<ExecutionOutcome, ExecutionError> {
    let request = FinishRunRequest {
        status: FinishStatus::Failed,
        error: Some(error.to_owned()),
        provider_session_id: None,
        usage: None,
        pricing_evidence: None,
        judgment: None,
        resume_at: None,
    };
    if finish_with_retry(connection, run_id, logs, &request, cancellation)? {
        Ok(ExecutionOutcome::Finished)
    } else {
        Ok(ExecutionOutcome::Cancelled)
    }
}

struct RunLogContext<'a> {
    client: &'a Client,
    runner_token: &'a str,
    cancellation: &'a CancellationToken,
    secrets: &'a [String],
}

fn run_harness(
    assignment: &mut PreparedAssignment,
    context: &RunLogContext<'_>,
    capabilities: &EffortCapabilities,
    timeout: Duration,
    report: &mut CodexRunReport,
) -> Result<ProcessOutput, String> {
    let RunLogContext {
        client,
        runner_token,
        cancellation,
        ..
    } = context;
    if cancellation.is_cancelled() {
        assignment.stop_log_delivery();
        return Err("assignment was canceled before process launch".to_owned());
    }
    let launch = CodexLaunch::for_assignment(assignment, capabilities)
        .map_err(|error| format!("could not prepare Codex command: {error}"))?;
    tracing::info!(
        run_id = %assignment.assignment().run.id,
        launch = %launch,
        "starting Codex harness"
    );
    assignment.buffer_launch_diagnostic(&launch.format_diagnostics());
    let mut command = launch.command();
    if cancellation.is_cancelled() {
        assignment.stop_log_delivery();
        return Err("assignment was canceled before process launch".to_owned());
    }
    let process = SupervisedProcess::spawn_with_output(&mut command)
        .map_err(|error| format!("could not start Codex harness: {error}"))?;
    let deadline = Instant::now() + timeout;

    if let Some(Err(error)) = retry_protocol_until(deadline, cancellation, || {
        assignment.harness_started_until(client, runner_token, deadline)
    }) {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not report that the Codex harness started"
        );
    }

    let started = Instant::now();
    let mut stdout = CodexStreamParser::default();
    let mut stdout_utf8 = Utf8StreamDecoder::default();
    let mut stderr_utf8 = Utf8StreamDecoder::default();
    let mut stderr_diagnostic = Vec::new();
    let mut stderr_truncated = false;
    let mut last_flush = Instant::now();
    let mut output = process
        .wait_timeout_with_output(
            deadline.saturating_duration_since(Instant::now()),
            TERMINATION_GRACE,
            || {
                if cancellation.is_cancelled() {
                    assignment.stop_log_delivery();
                }
                cancellation.is_cancelled() || assignment.log_delivery_cancelled()
            },
            |chunk| {
                if cancellation.is_cancelled() || assignment.log_delivery_cancelled() {
                    assignment.stop_log_delivery();
                    return;
                }
                if chunk.stream == ProcessStream::Stdout {
                    let text = stdout_utf8.push(&chunk.bytes);
                    append_events(report, assignment, context, stdout.push(&text), false);
                } else {
                    let remaining =
                        MAX_STDERR_DIAGNOSTIC_BYTES.saturating_sub(stderr_diagnostic.len());
                    let captured = chunk.bytes.len().min(remaining);
                    stderr_diagnostic.extend_from_slice(&chunk.bytes[..captured]);
                    stderr_truncated |= captured < chunk.bytes.len();
                    let text = stderr_utf8.push(&chunk.bytes);
                    append_stderr(assignment, context, &text);
                }
            },
            || {
                if cancellation.is_cancelled() || assignment.log_delivery_cancelled() {
                    assignment.stop_log_delivery();
                    return;
                }
                if last_flush.elapsed() >= Duration::from_secs(1) {
                    if let Err(error) = assignment.flush_logs(client, runner_token) {
                        tracing::warn!(
                            run_id = %assignment.assignment().run.id,
                            error = %error,
                            "could not flush live Codex run logs"
                        );
                    }
                    last_flush = Instant::now();
                }
            },
        )
        .map_err(|error| format!("could not wait for Codex harness: {error}"))?;
    if output.cancelled || cancellation.is_cancelled() || assignment.log_delivery_cancelled() {
        assignment.stop_log_delivery();
        return Ok(output);
    }
    let stdout_tail = stdout_utf8.finish();
    let mut stdout_events = stdout.push(&stdout_tail);
    stdout_events.extend(stdout.finish());
    append_events(report, assignment, context, stdout_events, false);
    let stderr_tail = stderr_utf8.finish();
    append_stderr(assignment, context, &stderr_tail);
    output.stderr = stderr_diagnostic;
    if stderr_truncated {
        output.stderr.extend_from_slice(b"\n[stderr truncated]");
    }
    if let Err(error) = assignment.append_exit_diagnostic(
        client,
        runner_token,
        &output.format_exit_diagnostic(started.elapsed()),
    ) {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not append Codex exit diagnostic"
        );
    }
    Ok(output)
}

fn append_stderr(assignment: &PreparedAssignment, context: &RunLogContext<'_>, text: &str) {
    if text.is_empty() || context.cancellation.is_cancelled() || assignment.log_delivery_cancelled()
    {
        return;
    }
    let rendered = redact(text, context.secrets);
    if let Err(error) = assignment.append_harness_output(
        context.client,
        context.runner_token,
        &format!("[stderr] {rendered}"),
    ) {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not append Codex stderr to the Tines run log"
        );
    }
}

fn append_events(
    report: &mut CodexRunReport,
    assignment: &PreparedAssignment,
    context: &RunLogContext<'_>,
    events: Vec<CodexEvent>,
    stderr: bool,
) {
    let RunLogContext {
        client,
        runner_token,
        cancellation,
        secrets,
    } = context;
    if cancellation.is_cancelled() || assignment.log_delivery_cancelled() {
        assignment.stop_log_delivery();
        return;
    }
    let mut lines = Vec::new();
    for event in events {
        report.observe(&event);
        lines.extend(event.render_lines().into_iter().map(|line| {
            if stderr {
                format!("[stderr] {line}")
            } else {
                line
            }
        }));
    }

    let rendered = lines
        .into_iter()
        .map(|line| redact(&line, secrets))
        .collect::<Vec<_>>()
        .join("\n");
    if rendered.is_empty() {
        return;
    }

    if let Err(error) =
        assignment.append_harness_output(client, runner_token, &format!("{rendered}\n"))
    {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not append Codex output to the Tines run log"
        );
    }
}

#[derive(Default)]
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut decoded = String::new();
        let mut consumed = 0;
        loop {
            let remaining = &self.pending[consumed..];
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    consumed = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    decoded.push_str(
                        std::str::from_utf8(&self.pending[consumed..valid_end])
                            .expect("valid UTF-8 prefix"),
                    );
                    consumed = valid_end;
                    if let Some(error_len) = error.error_len() {
                        decoded.push('\u{fffd}');
                        consumed += error_len;
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        decoded
    }

    fn finish(&mut self) -> String {
        let decoded = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        decoded
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
    logs: &crate::protocol::client::RunLogBuffer,
    request: &FinishRunRequest,
    cancellation: &CancellationToken,
) -> Result<bool, ExecutionError> {
    let mut failures = 0u32;
    loop {
        if cancellation.is_cancelled() || logs.is_cancelled() {
            return Ok(false);
        }
        match connection.finish_assignment_with_logs(run_id, logs, request) {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => return Ok(false),
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
    use super::{RunLogContext, Utf8StreamDecoder, run_harness};
    use crate::assignment::{PreparedAssignment, resolve_assignment};
    use crate::cancellation::CancellationToken;
    use crate::config::Config;
    use crate::effort::EffortCapabilities;
    use crate::finish::CodexRunReport;
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
    fn utf8_decoder_keeps_multibyte_characters_split_across_process_chunks() {
        let source = "Codex 🛰️ output";
        let mut decoder = Utf8StreamDecoder::default();
        let mut decoded = String::new();
        for byte in source.as_bytes() {
            decoded.push_str(&decoder.push(&[*byte]));
        }
        decoded.push_str(&decoder.finish());
        assert_eq!(decoded, source);
    }

    #[test]
    fn retrying_harness_start_log_does_not_extend_the_process_timeout() {
        let directory = TestDirectory::new();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
        let address = listener.local_addr().expect("read fake Tines address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let mut log_requests = 0;
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept fake Tines request");
                let request = read_request(&mut stream);
                if request.starts_with("GET /api/v1/issues/iss_timeout ") {
                    respond(
                        &mut stream,
                        200,
                        r#"{"id":"iss_timeout","workflow":{"name":"Implementation"}}"#,
                    );
                } else if request.starts_with("POST /api/v1/runs/arun_timeout/logs ") {
                    log_requests += 1;
                    if log_requests == 1 {
                        respond(
                            &mut stream,
                            503,
                            r#"{"error":{"code":"unavailable","message":"retry"}}"#,
                        );
                    } else {
                        respond(&mut stream, 200, r#"{"log_seq":1}"#);
                    }
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
        let mut report = CodexRunReport::new(None);
        let cancellation = CancellationToken::default();
        let log_context = RunLogContext {
            client: &client,
            runner_token: "runner-token",
            cancellation: &cancellation,
            secrets: &[],
        };
        let output = run_harness(
            &mut prepared,
            &log_context,
            &capabilities,
            Duration::from_millis(200),
            &mut report,
        )
        .expect("supervise harness after start-log outage");

        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
        let requests = server.join().expect("join fake Tines server");
        assert!(requests[0].starts_with("GET /api/v1/issues/iss_timeout "));
        assert!(requests[1].starts_with("POST /api/v1/runs/arun_timeout/logs "));
        assert!(requests[2].starts_with("POST /api/v1/runs/arun_timeout/logs "));
        let first_body = requests[1].split_once("\r\n\r\n").unwrap().1;
        let retry_body = requests[2].split_once("\r\n\r\n").unwrap().1;
        assert_eq!(first_body, retry_body, "retry must preserve chunk and seq");
        assert!(first_body.contains("\"seq\":1"));
    }
}
