//! Run prepared assignments and settle their outcomes with Tines.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::assignment::PreparedAssignment;
use crate::cancellation::CancellationToken;
use crate::config::{RunKeyDelivery, WorkspaceRetention};
use crate::executor_events::ExecutorEventStream;
use crate::executor_transport::{ExecutorTransport, execution_request};
use crate::protocol::client::{Client, ErrorCategory, RunLogBuffer};
use crate::protocol::{FinishJudgment, FinishRunRequest, FinishStatus};
use crate::recovery::ActiveRunStore;
use crate::runner::{RunnerConnection, RunnerError};
use crate::shutdown::ShutdownSignal;

const MAX_BACKOFF: Duration = Duration::from_secs(60);
// Allow the executor to receive shutdown and terminate its own harness group
// before the daemon escalates to SIGKILL. The executor has a two-second grace.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);
const LIVE_LOG_RETRY_WINDOW: Duration = Duration::from_millis(500);

/// Shared shutdown signal and durable run inventory for an assignment worker.
#[derive(Clone, Copy)]
pub struct ExecutionContext<'a> {
    pub shutdown: &'a ShutdownSignal,
    pub active_runs: &'a ActiveRunStore,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(shutdown: &'a ShutdownSignal, active_runs: &'a ActiveRunStore) -> Self {
        Self {
            shutdown,
            active_runs,
        }
    }
}

/// Execute one assignment and report its outcome to Tines.
///
/// The executor owns workspace creation, retention, and cleanup.
pub fn execute_assignment(
    assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    retention: &WorkspaceRetention,
    context: &ExecutionContext<'_>,
) -> Result<(), ExecutionError> {
    execute_assignment_cancellable(
        assignment,
        connection,
        client,
        retention,
        &CancellationToken::default(),
        context,
    )
    .map(|_| ())
}

/// Execute one assignment while honoring supervisor cancellation at every
/// process, log, and finish boundary.
pub fn execute_assignment_cancellable(
    assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    retention: &WorkspaceRetention,
    cancellation: &CancellationToken,
    context: &ExecutionContext<'_>,
) -> Result<ExecutionOutcome, ExecutionError> {
    execute_assignment_cancellable_inner(
        assignment,
        connection,
        client,
        retention,
        cancellation,
        context,
    )
}

fn execute_assignment_cancellable_inner(
    assignment: PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    retention: &WorkspaceRetention,
    cancellation: &CancellationToken,
    context: &ExecutionContext<'_>,
) -> Result<ExecutionOutcome, ExecutionError> {
    if cancellation.is_cancelled() {
        return settle_canceled_assignment(&assignment, context.active_runs);
    }
    let run_id = assignment.assignment().run.id.clone();
    let timeout = Duration::from_secs(assignment.assignment().timeout_minutes.saturating_mul(60));
    let output = run_executor(
        &assignment,
        connection,
        client,
        retention,
        context.active_runs,
        context.shutdown,
        cancellation,
        timeout,
    );

    if cancellation.is_cancelled() || assignment.log_delivery_cancelled() || output.cancelled {
        return settle_canceled_assignment(&assignment, context.active_runs);
    }
    if cancellation.is_cancelled() {
        return settle_canceled_assignment(&assignment, context.active_runs);
    }
    let finish_request = output.stream.finish_request(
        output.failure.as_deref(),
        &output.stderr,
        output.interrupted,
    );
    if !finish_with_retry(
        connection,
        &run_id,
        &assignment.run_log_buffer(),
        &finish_request,
        cancellation,
    )? {
        return settle_canceled_assignment(&assignment, context.active_runs);
    }
    context
        .active_runs
        .remove(&run_id)
        .map_err(ExecutionError::ActiveStateCleanup)?;
    Ok(ExecutionOutcome::Finished)
}

struct ExecutorExecution {
    stream: ExecutorEventStream,
    failure: Option<String>,
    stderr: String,
    cancelled: bool,
    interrupted: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_executor(
    assignment: &PreparedAssignment,
    connection: &RunnerConnection,
    client: &Client,
    retention: &WorkspaceRetention,
    active_runs: &ActiveRunStore,
    shutdown: &ShutdownSignal,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> ExecutorExecution {
    let resolved = assignment.resolved();
    let config = &resolved.resolution().config;
    let request = execution_request(resolved, assignment.api_url(), retention);
    let environment_run_key = match config.run_key_delivery {
        RunKeyDelivery::Request => None,
        RunKeyDelivery::Environment => assignment.assignment().run_key.as_deref(),
    };
    let additional_secrets = environment_run_key.into_iter().collect::<Vec<_>>();
    let mut stream = ExecutorEventStream::new_with_secrets(&request, &additional_secrets);
    let transport = ExecutorTransport::from_resolved(config);
    let run_id = assignment.assignment().run.id.clone();
    let runner_token = connection.credentials().runner_token().to_owned();
    let run_logs = assignment.run_log_buffer();
    let deadline = Cell::new(None::<Instant>);
    let last_flush = Cell::new(Instant::now());
    let protocol_failure = RefCell::new(None::<String>);

    let result = transport.run_with_callbacks_and_environment_key(
        &request,
        environment_run_key,
        timeout,
        TERMINATION_GRACE,
        || {
            if cancellation.is_cancelled() {
                run_logs.stop_sending();
            }
            cancellation.is_cancelled() || run_logs.is_cancelled()
        },
        || shutdown.is_requested(),
        |identity, run_deadline| {
            active_runs
                .record_transport(run_id.clone(), identity.clone(), None)
                .map_err(|error| error.to_string())?;
            deadline.set(Some(run_deadline));
            last_flush.set(Instant::now());
            if let Err(error) =
                run_logs.harness_started_until(client, &run_id, &runner_token, run_deadline)
            {
                tracing::warn!(
                    run_id,
                    error = %error,
                    "could not report that the executor started"
                );
            }
            Ok(())
        },
        |chunk| {
            if cancellation.is_cancelled() || run_logs.is_cancelled() {
                run_logs.stop_sending();
                return;
            }
            if protocol_failure.borrow().is_some() {
                return;
            }
            match stream.push(chunk) {
                Ok(logs) => {
                    let run_deadline = deadline.get().unwrap_or_else(|| Instant::now() + timeout);
                    append_executor_logs(
                        &run_logs,
                        client,
                        &run_id,
                        &runner_token,
                        logs,
                        run_deadline,
                    );
                }
                Err(error) => {
                    let error_message = error.to_string();
                    let run_deadline = deadline.get().unwrap_or_else(|| Instant::now() + timeout);
                    append_executor_logs(
                        &run_logs,
                        client,
                        &run_id,
                        &runner_token,
                        error.logs,
                        run_deadline,
                    );
                    tracing::warn!(
                        run_id,
                        error = %error_message,
                        "executor emitted invalid protocol output"
                    );
                    *protocol_failure.borrow_mut() =
                        Some(format!("executor protocol failure: {error_message}"));
                }
            }
        },
        || {
            if cancellation.is_cancelled() || run_logs.is_cancelled() {
                run_logs.stop_sending();
                return;
            }
            if last_flush.get().elapsed() < Duration::from_secs(1) {
                return;
            }
            let run_deadline = deadline.get().unwrap_or_else(|| Instant::now() + timeout);
            if let Err(error) = run_logs.flush_until(
                client,
                &run_id,
                &runner_token,
                live_log_deadline(run_deadline),
            ) {
                tracing::warn!(
                    run_id,
                    error = %error,
                    "could not flush live executor logs"
                );
            }
            last_flush.set(Instant::now());
        },
    );

    let mut failure = protocol_failure.into_inner();
    let (stderr, cancelled, interrupted) = match result {
        Ok(output) => {
            if !output.stderr.trim().is_empty() {
                tracing::warn!(
                    run_id,
                    diagnostic = %output.stderr,
                    "executor wrote to its diagnostic stream"
                );
            }
            if output.timed_out {
                failure.get_or_insert_with(|| {
                    format!(
                        "executor exceeded the {}-minute run timeout",
                        assignment.assignment().timeout_minutes
                    )
                });
            }
            if output.interrupted {
                failure.get_or_insert_with(|| "runner shutdown interrupted the executor".into());
            }
            if !output.timed_out
                && !output.interrupted
                && !output.cancelled
                && failure.is_none()
                && let Err(error) = stream.finish()
            {
                failure = Some(format!("executor protocol failure: {error}"));
            }
            (output.stderr, output.cancelled, output.interrupted)
        }
        Err(error) => {
            failure.get_or_insert_with(|| error.to_string());
            (
                String::new(),
                cancellation.is_cancelled(),
                shutdown.is_requested(),
            )
        }
    };

    ExecutorExecution {
        stream,
        failure,
        stderr,
        cancelled,
        interrupted,
    }
}

fn append_executor_logs(
    logs: &RunLogBuffer,
    client: &Client,
    run_id: &str,
    runner_token: &str,
    chunks: Vec<String>,
    deadline: Instant,
) {
    for chunk in chunks {
        if logs.is_cancelled() {
            logs.stop_sending();
            return;
        }
        if let Err(error) = logs.append_harness_output_until(
            client,
            run_id,
            runner_token,
            &chunk,
            live_log_deadline(deadline),
        ) {
            tracing::warn!(
                run_id,
                error = %error,
                "could not append executor output to the Tines run log"
            );
        }
    }
}

fn settle_canceled_assignment(
    assignment: &PreparedAssignment,
    active_runs: &ActiveRunStore,
) -> Result<ExecutionOutcome, ExecutionError> {
    assignment.stop_log_delivery();
    active_runs
        .remove(&assignment.assignment().run.id)
        .map_err(ExecutionError::ActiveStateCleanup)?;
    Ok(ExecutionOutcome::Cancelled)
}

/// The terminal action taken for one assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    Finished,
    Cancelled,
}

/// Report a pre-execution interruption unless cancellation has settled the run.
pub fn report_preparation_failure(
    connection: &RunnerConnection,
    run_id: &str,
    logs: &crate::protocol::client::RunLogBuffer,
    error: &str,
    cancellation: &CancellationToken,
    context: &ExecutionContext<'_>,
) -> Result<ExecutionOutcome, ExecutionError> {
    let request = FinishRunRequest {
        status: FinishStatus::Failed,
        error: Some(error.to_owned()),
        provider_session_id: None,
        usage: None,
        pricing_evidence: None,
        judgment: context
            .shutdown
            .is_requested()
            .then_some(FinishJudgment::Interrupted),
        resume_at: None,
    };
    if finish_with_retry(connection, run_id, logs, &request, cancellation)? {
        Ok(ExecutionOutcome::Finished)
    } else {
        Ok(ExecutionOutcome::Cancelled)
    }
}

fn live_log_deadline(run_deadline: Instant) -> Instant {
    (Instant::now() + LIVE_LOG_RETRY_WINDOW).min(run_deadline)
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

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(6)).min(MAX_BACKOFF)
}

/// An error that prevented local settlement of a completed harness attempt.
#[derive(Debug)]
pub enum ExecutionError {
    FinishReport(RunnerError),
    ActiveStateCleanup(io::Error),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FinishReport(error) => write!(f, "could not report run finish: {error}"),
            Self::ActiveStateCleanup(error) => {
                write!(
                    f,
                    "run settled, but active-run state could not be cleared: {error}"
                )
            }
        }
    }
}

impl Error for ExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FinishReport(error) => Some(error),
            Self::ActiveStateCleanup(error) => Some(error),
        }
    }
}
