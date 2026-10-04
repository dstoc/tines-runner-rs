//! Cancellable preparation and execution for one server-delivered assignment.

use crate::assignment::resolve_assignment;
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::effort::{EffortCapabilities, assignment_effort_rejection};
use crate::execution::{self, ExecutionOutcome};
use crate::executor_capabilities::ExecutorCapabilities;
use crate::executor_transport::ExecutorTransport;
use crate::protocol::RunnerAssignment;
use crate::protocol::client::Client;
use crate::runner::RunnerConnection;
use crate::workspace::{MaterializedWorkspace, WorkspaceError};

/// Result of preparing and running one assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssignmentTaskOutcome {
    Finished,
    Cancelled,
    Declined(String),
}

/// Enrich metadata, materialize the workspace, and execute one assignment.
/// The poll loop remains free to receive cancellation while these steps run.
#[allow(clippy::too_many_arguments)]
pub fn run_assignment(
    config: &Config,
    connection: &RunnerConnection,
    issue_client: &Client,
    assignment: RunnerAssignment,
    run_logs: crate::protocol::client::RunLogBuffer,
    default_executor: &ExecutorTransport,
    advertised_capabilities: &ExecutorCapabilities,
    legacy_launch_capabilities: &EffortCapabilities,
    cancellation: &CancellationToken,
    context: &execution::ExecutionContext<'_>,
) -> Result<AssignmentTaskOutcome, String> {
    run_assignment_inner(
        AssignmentContext {
            config,
            connection,
            issue_client,
            default_executor,
            advertised_capabilities,
            legacy_launch_capabilities,
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
    default_executor: &'a ExecutorTransport,
    advertised_capabilities: &'a ExecutorCapabilities,
    legacy_launch_capabilities: &'a EffortCapabilities,
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
        default_executor,
        advertised_capabilities,
        legacy_launch_capabilities,
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

    let executor = ExecutorTransport::from_resolved(&resolved.resolution().config);
    let capabilities = if &executor == default_executor {
        advertised_capabilities.clone()
    } else {
        executor
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
    let legacy_launch_capabilities = if resolved.resolution().config.wrapper == config.wrapper {
        legacy_launch_capabilities.clone()
    } else {
        EffortCapabilities::discover_with_wrapper(
            &resolved.resolution().config.wrapper,
            crate::VERSION,
        )
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
    };
    if !capabilities.supports(harness) {
        return Ok(AssignmentTaskOutcome::Declined(format!(
            "configured executor does not verify support for the {harness} harness"
        )));
    }
    if !legacy_launch_capabilities.supports_harness(harness) {
        return Ok(AssignmentTaskOutcome::Declined(format!(
            "local legacy launcher does not verify support for the {harness} harness"
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
    if let Some(reason) = assignment_effort_rejection(&assignment, &legacy_launch_capabilities) {
        if context.shutdown.is_requested() {
            return report_interrupted_before_execution(
                connection,
                &assignment.run.id,
                &run_logs,
                cancellation,
                context,
            );
        }
        return Ok(AssignmentTaskOutcome::Declined(format!(
            "local legacy launcher: {reason}"
        )));
    }

    let run_id = assignment.run.id.clone();
    let workspace = match MaterializedWorkspace::create_cancellable_with_workspace_hook(
        &resolved.resolution().config.workspace_parent,
        resolved.assignment(),
        &config.server_url,
        cancellation,
        |path| context.active_runs.record_workspace(run_id.clone(), path),
        |chunk| {
            tracing::info!(
                run_id = %assignment.run.id,
                git_output = %chunk.trim_end(),
                "repository checkout progress"
            );
            run_logs.buffer_preparation_output(chunk);
        },
    ) {
        Ok(workspace) => workspace,
        Err(WorkspaceError::Cancelled) => {
            context
                .active_runs
                .remove(&run_id)
                .map_err(|error| error.to_string())?;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(error) if cancellation.is_cancelled() => {
            if error.workspace_cleanup_failed() {
                return Err(format!(
                    "assignment was canceled, but workspace cleanup failed; active state was retained: {error}"
                ));
            }
            context
                .active_runs
                .remove(&run_id)
                .map_err(|error| error.to_string())?;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(error) => {
            let output = run_logs.preparation_output();
            let failure = if output.is_empty() {
                error.to_string()
            } else {
                format!("{error}\nRepository checkout output:\n{output}")
            };
            let outcome = execution::report_preparation_failure(
                connection,
                &assignment.run.id,
                &run_logs,
                &failure,
                cancellation,
                context,
            )
            .map_err(|error| error.to_string())?;
            if error.workspace_cleanup_failed() {
                return Err(format!(
                    "assignment finish was reported, but workspace cleanup failed; active state was retained: {error}"
                ));
            }
            context
                .active_runs
                .remove(&run_id)
                .map_err(|error| error.to_string())?;
            return Ok(match outcome {
                ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
                ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
            });
        }
    };
    if cancellation.is_cancelled() {
        workspace
            .cleanup()
            .map_err(|error| format!("could not clean canceled workspace: {error}"))?;
        context
            .active_runs
            .remove(&run_id)
            .map_err(|error| error.to_string())?;
        return Ok(AssignmentTaskOutcome::Cancelled);
    }

    tracing::info!(
        run_id = %assignment.run.id,
        project = resolved.context().project(),
        workflow = resolved.context().workflow(),
        state = resolved.context().state(),
        matched_overrides = ?resolved.resolution().matching_overrides(),
        workspace = %workspace.path().display(),
        "assignment workspace materialized and queued"
    );
    let prepared = crate::assignment::PreparedAssignment::new(resolved, workspace)
        .with_run_log_buffer(run_logs);
    execution::execute_assignment_cancellable(
        prepared,
        connection,
        issue_client,
        &legacy_launch_capabilities,
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
