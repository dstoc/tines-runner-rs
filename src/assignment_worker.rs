//! Cancellable preparation and execution for one server-delivered assignment.

use crate::assignment::resolve_assignment;
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::effort::assignment_effort_rejection;
use crate::execution::{self, ExecutionOutcome};
use crate::executor_capabilities::ExecutorCapabilities;
use crate::executor_transport::ExecutorTransport;
use crate::protocol::RunnerAssignment;
use crate::protocol::client::Client;
use crate::runner::RunnerConnection;

/// Result of preparing and running one assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssignmentTaskOutcome {
    Finished,
    Cancelled,
    Declined(String),
}

/// Enrich assignment metadata and execute one assignment through the executor.
/// The poll loop remains free to receive cancellation while these steps run.
#[allow(clippy::too_many_arguments)]
pub fn run_assignment(
    config: &Config,
    connection: &RunnerConnection,
    issue_client: &Client,
    assignment: RunnerAssignment,
    run_logs: crate::protocol::client::RunLogBuffer,
    default_capabilities_transport: &ExecutorTransport,
    advertised_capabilities: &ExecutorCapabilities,
    cancellation: &CancellationToken,
    context: &execution::ExecutionContext<'_>,
) -> Result<AssignmentTaskOutcome, String> {
    run_assignment_inner(
        AssignmentContext {
            config,
            connection,
            issue_client,
            default_capabilities_transport,
            advertised_capabilities,
            cancellation,
            context,
        },
        assignment,
        run_logs,
    )
}

struct AssignmentContext<'a> {
    config: &'a Config,
    connection: &'a RunnerConnection,
    issue_client: &'a Client,
    default_capabilities_transport: &'a ExecutorTransport,
    advertised_capabilities: &'a ExecutorCapabilities,
    cancellation: &'a CancellationToken,
    context: &'a execution::ExecutionContext<'a>,
}

fn run_assignment_inner(
    context: AssignmentContext<'_>,
    assignment: RunnerAssignment,
    run_logs: crate::protocol::client::RunLogBuffer,
) -> Result<AssignmentTaskOutcome, String> {
    let AssignmentContext {
        config,
        connection,
        issue_client,
        default_capabilities_transport,
        advertised_capabilities,
        cancellation,
        context,
    } = context;
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }
    if context.shutdown.is_requested() {
        return report_interrupted_before_execution(
            connection,
            &assignment.run.id,
            &run_logs,
            cancellation,
            context,
        );
    }

    let resolved = match resolve_assignment(config, issue_client, &assignment) {
        Ok(resolved) => resolved,
        Err(error) if cancellation.is_cancelled() => {
            let _ = error;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(_error) if context.shutdown.is_requested() => {
            return report_interrupted_before_execution(
                connection,
                &assignment.run.id,
                &run_logs,
                cancellation,
                context,
            );
        }
        Err(error) => return Ok(AssignmentTaskOutcome::Declined(error.to_string())),
    };
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }
    if context.shutdown.is_requested() {
        return report_interrupted_before_execution(
            connection,
            &assignment.run.id,
            &run_logs,
            cancellation,
            context,
        );
    }

    let capabilities_transport =
        ExecutorTransport::for_resolved_capabilities(&resolved.resolution().config);
    let capabilities = if &capabilities_transport == default_capabilities_transport {
        advertised_capabilities.clone()
    } else {
        capabilities_transport
            .discover_capabilities()
            .unwrap_or_else(|error| ExecutorCapabilities::unavailable(error.to_string()))
    };
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }
    if context.shutdown.is_requested() {
        return report_interrupted_before_execution(
            connection,
            &assignment.run.id,
            &run_logs,
            cancellation,
            context,
        );
    }
    let harness = match resolved.resolution().config.runner_type {
        crate::config::RunnerType::Codex => "codex",
        crate::config::RunnerType::Custom => "custom",
    };
    if !capabilities.supports(harness) {
        return Ok(AssignmentTaskOutcome::Declined(format!(
            "configured executor does not verify support for the {harness} harness"
        )));
    }
    let effort_capabilities = capabilities.effort_report(harness, crate::VERSION);
    if let Some(reason) = assignment_effort_rejection(&assignment, &effort_capabilities) {
        if context.shutdown.is_requested() {
            return report_interrupted_before_execution(
                connection,
                &assignment.run.id,
                &run_logs,
                cancellation,
                context,
            );
        }
        return Ok(AssignmentTaskOutcome::Declined(reason));
    }
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }
    tracing::info!(
        run_id = %assignment.run.id,
        project = resolved.context().project(),
        workflow = resolved.context().workflow(),
        state = resolved.context().state(),
        matched_overrides = ?resolved.resolution().matching_overrides(),
        "assignment resolved for executor"
    );
    let prepared =
        crate::assignment::PreparedAssignment::new(resolved).with_run_log_buffer(run_logs);
    execution::execute_assignment_cancellable(
        prepared,
        connection,
        issue_client,
        &config.workspace_retention,
        cancellation,
        context,
    )
    .map(|outcome| match outcome {
        ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
        ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
    })
    .map_err(|error| error.to_string())
}

fn report_interrupted_before_execution(
    connection: &RunnerConnection,
    run_id: &str,
    logs: &crate::protocol::client::RunLogBuffer,
    cancellation: &CancellationToken,
    context: &execution::ExecutionContext<'_>,
) -> Result<AssignmentTaskOutcome, String> {
    execution::report_preparation_failure(
        connection,
        run_id,
        logs,
        "runner shutdown interrupted the run before execution",
        cancellation,
        context,
    )
    .map(|outcome| match outcome {
        ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
        ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
    })
    .map_err(|error| error.to_string())
}
