//! Run prepared assignments and settle their outcomes with Tines.

use std::error::Error;
use std::fmt;
use std::io;
use std::thread;
use std::time::Duration;

use crate::assignment::PreparedAssignment;
use crate::codex::CodexLaunch;
use crate::codex_stream::CodexStreamParser;
use crate::effort::EffortCapabilities;
use crate::finish::CodexRunReport;
use crate::process::{ProcessExit, ProcessOutput, SupervisedProcess};
use crate::protocol::client::{Client, ClientError, ErrorCategory};
use crate::protocol::{FinishRunRequest, FinishStatus};
use crate::runner::{RunnerConnection, RunnerError};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Execute one assignment, report its outcome, then remove its workspace.
///
/// A retryable finish error keeps the workspace in place until Tines accepts
/// the report. A non-retryable error also leaves it in place for recovery.
pub fn execute_assignment(
    mut assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    capabilities: &EffortCapabilities,
) -> Result<(), ExecutionError> {
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
    let run_output = run_harness(&mut assignment, client, runner_token, capabilities);

    let (status, error, output) = match run_output {
        Ok(output) if !output.timed_out && output.exit == ProcessExit::Code(0) => {
            (FinishStatus::Completed, None, Some(output))
        }
        Ok(output) => {
            let error = process_error(&output, assignment.assignment().timeout_minutes);
            (FinishStatus::Failed, Some(error), Some(output))
        }
        Err(error) => (FinishStatus::Failed, Some(error), None),
    };
    let error = error.map(|error| redact(&error, &secrets));

    if let Some(output) = output {
        collect_output(
            &mut report,
            &mut assignment,
            client,
            runner_token,
            &secrets,
            &output,
        );
    }

    let finish_request = report.into_finish_request(status, error);
    finish_with_retry(connection, &run_id, &finish_request)?;
    assignment
        .workspace()
        .cleanup()
        .map_err(ExecutionError::WorkspaceCleanup)
}

fn run_harness(
    assignment: &mut PreparedAssignment,
    client: &Client,
    runner_token: &str,
    capabilities: &EffortCapabilities,
) -> Result<ProcessOutput, String> {
    let launch = CodexLaunch::for_assignment(assignment, capabilities)
        .map_err(|error| format!("could not prepare Codex command: {error}"))?;
    tracing::info!(
        run_id = %assignment.assignment().run.id,
        launch = %launch,
        "starting Codex harness"
    );
    let mut command = launch.command();
    let process = SupervisedProcess::spawn(&mut command)
        .map_err(|error| format!("could not start Codex harness: {error}"))?;

    if let Err(error) = retry_protocol(|| assignment.harness_started(client, runner_token)) {
        tracing::warn!(
            run_id = %assignment.assignment().run.id,
            error = %error,
            "could not report that the Codex harness started"
        );
    }

    let timeout = Duration::from_secs(assignment.assignment().timeout_minutes.saturating_mul(60));
    process
        .wait_timeout(timeout, TERMINATION_GRACE)
        .map_err(|error| format!("could not wait for Codex harness: {error}"))
}

fn collect_output(
    report: &mut CodexRunReport,
    assignment: &mut PreparedAssignment,
    client: &Client,
    runner_token: &str,
    secrets: &[String],
    output: &ProcessOutput,
) {
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

    if let Err(error) = retry_protocol(|| {
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
) -> Result<(), ExecutionError> {
    let mut failures = 0u32;
    loop {
        match connection.finish_assignment(run_id, request) {
            Ok(_) => return Ok(()),
            Err(RunnerError::Protocol(error)) if error.category() == ErrorCategory::Retryable => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                tracing::warn!(
                    run_id,
                    error = %error,
                    backoff_seconds = delay.as_secs(),
                    "finish report failed; retrying"
                );
                thread::sleep(delay);
            }
            Err(error) => return Err(ExecutionError::FinishReport(error)),
        }
    }
}

fn retry_protocol<T>(
    mut request: impl FnMut() -> Result<T, ClientError>,
) -> Result<T, ClientError> {
    let mut failures = 0u32;
    loop {
        match request() {
            Ok(response) => return Ok(response),
            Err(error) if error.category() == ErrorCategory::Retryable => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                thread::sleep(delay);
            }
            Err(error) => return Err(error),
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
        }
    }
}

impl Error for ExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FinishReport(error) => Some(error),
            Self::WorkspaceCleanup(error) => Some(error),
        }
    }
}
