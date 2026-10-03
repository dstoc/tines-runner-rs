//! Cancellable preparation and execution for one server-delivered assignment.

use crate::assignment::resolve_assignment;
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::effort::{EffortCapabilities, assignment_effort_rejection};
use crate::execution::{self, ExecutionOutcome};
use crate::protocol::RunnerAssignment;
use crate::protocol::client::Client;
use crate::recovery::ActiveRunStore;
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
pub fn run_assignment(
    config: &Config,
    connection: &RunnerConnection,
    issue_client: &Client,
    assignment: RunnerAssignment,
    run_logs: crate::protocol::client::RunLogBuffer,
    advertised_capabilities: &EffortCapabilities,
    cancellation: &CancellationToken,
) -> Result<AssignmentTaskOutcome, String> {
    run_assignment_inner(
        AssignmentContext {
            config,
            connection,
            issue_client,
            advertised_capabilities,
            cancellation,
            active_runs: None,
        },
        assignment,
        run_logs,
    )
}

/// Run an assignment while persisting its workspace and harness identity.
#[allow(clippy::too_many_arguments)]
pub fn run_assignment_with_active_runs(
    config: &Config,
    connection: &RunnerConnection,
    issue_client: &Client,
    assignment: RunnerAssignment,
    run_logs: crate::protocol::client::RunLogBuffer,
    advertised_capabilities: &EffortCapabilities,
    cancellation: &CancellationToken,
    active_runs: &ActiveRunStore,
) -> Result<AssignmentTaskOutcome, String> {
    run_assignment_inner(
        AssignmentContext {
            config,
            connection,
            issue_client,
            advertised_capabilities,
            cancellation,
            active_runs: Some(active_runs),
        },
        assignment,
        run_logs,
    )
}

struct AssignmentContext<'a> {
    config: &'a Config,
    connection: &'a RunnerConnection,
    issue_client: &'a Client,
    advertised_capabilities: &'a EffortCapabilities,
    cancellation: &'a CancellationToken,
    active_runs: Option<&'a ActiveRunStore>,
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
        advertised_capabilities,
        cancellation,
        active_runs,
    } = context;
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }

    let capabilities = if assignment.effort.is_some() {
        EffortCapabilities::discover(crate::VERSION)
    } else {
        advertised_capabilities.clone()
    };
    if let Some(reason) = assignment_effort_rejection(&assignment, &capabilities) {
        return Ok(AssignmentTaskOutcome::Declined(reason));
    }

    let resolved = match resolve_assignment(config, issue_client, &assignment) {
        Ok(resolved) => resolved,
        Err(error) if cancellation.is_cancelled() => {
            let _ = error;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(error) => return Ok(AssignmentTaskOutcome::Declined(error.to_string())),
    };
    if cancellation.is_cancelled() {
        return Ok(AssignmentTaskOutcome::Cancelled);
    }

    let run_id = assignment.run.id.clone();
    let workspace = match MaterializedWorkspace::create_cancellable_with_workspace_hook(
        &resolved.resolution().config.workspace_parent,
        resolved.assignment(),
        &config.server_url,
        cancellation,
        |path| match active_runs {
            Some(active_runs) => active_runs.record_workspace(run_id.clone(), path),
            None => Ok(()),
        },
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
            remove_active_run(active_runs, &run_id)?;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(error) if cancellation.is_cancelled() => {
            if error.workspace_cleanup_failed() {
                return Err(format!(
                    "assignment was canceled, but workspace cleanup failed; active state was retained: {error}"
                ));
            }
            remove_active_run(active_runs, &run_id)?;
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
            )
            .map_err(|error| error.to_string())?;
            if error.workspace_cleanup_failed() {
                return Err(format!(
                    "assignment finish was reported, but workspace cleanup failed; active state was retained: {error}"
                ));
            }
            remove_active_run(active_runs, &run_id)?;
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
        remove_active_run(active_runs, &run_id)?;
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
    let execution_result = match active_runs {
        Some(active_runs) => execution::execute_assignment_cancellable_with_active_runs(
            prepared,
            connection,
            issue_client,
            &capabilities,
            &config.workspace_retention,
            cancellation,
            active_runs,
        ),
        None => execution::execute_assignment_cancellable(
            prepared,
            connection,
            issue_client,
            &capabilities,
            &config.workspace_retention,
            cancellation,
        ),
    };
    execution_result
        .map(|outcome| match outcome {
            ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
            ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
        })
        .map_err(|error| error.to_string())
}

fn remove_active_run(active_runs: Option<&ActiveRunStore>, run_id: &str) -> Result<(), String> {
    if let Some(active_runs) = active_runs {
        active_runs
            .remove(run_id)
            .map_err(|error| format!("could not clear settled active-run state: {error}"))?;
    }
    Ok(())
}
