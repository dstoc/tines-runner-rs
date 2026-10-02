//! Cancellable preparation and execution for one server-delivered assignment.

use crate::assignment::resolve_assignment;
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::effort::{EffortCapabilities, assignment_effort_rejection};
use crate::execution::{self, ExecutionOutcome};
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
pub fn run_assignment(
    config: &Config,
    connection: &RunnerConnection,
    issue_client: &Client,
    assignment: RunnerAssignment,
    advertised_capabilities: &EffortCapabilities,
    cancellation: &CancellationToken,
) -> Result<AssignmentTaskOutcome, String> {
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

    let mut run_logs = crate::protocol::client::RunLogBuffer::new();
    let workspace = match MaterializedWorkspace::create_cancellable_with_git_log(
        &resolved.resolution().config.workspace_parent,
        resolved.assignment(),
        &config.server_url,
        cancellation,
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
        Err(WorkspaceError::Cancelled) => return Ok(AssignmentTaskOutcome::Cancelled),
        Err(error) if cancellation.is_cancelled() => {
            let _ = error;
            return Ok(AssignmentTaskOutcome::Cancelled);
        }
        Err(error) => {
            let output = run_logs.preparation_output();
            let failure = if output.is_empty() {
                error.to_string()
            } else {
                format!("{error}\nRepository checkout output:\n{output}")
            };
            return execution::report_preparation_failure(
                connection,
                &assignment.run.id,
                &failure,
                cancellation,
            )
            .map(|outcome| match outcome {
                ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
                ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
            })
            .map_err(|error| error.to_string());
        }
    };
    if cancellation.is_cancelled() {
        workspace
            .cleanup()
            .map_err(|error| format!("could not clean canceled workspace: {error}"))?;
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
        &capabilities,
        cancellation,
    )
    .map(|outcome| match outcome {
        ExecutionOutcome::Finished => AssignmentTaskOutcome::Finished,
        ExecutionOutcome::Cancelled => AssignmentTaskOutcome::Cancelled,
    })
    .map_err(|error| error.to_string())
}
