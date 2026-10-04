//! Executor-side preparation of one assignment workspace.

use std::error::Error;
use std::fmt;
use url::Url;

use crate::config::{self, ConfigError};
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRequest, ProtocolError, TerminalResult,
    TerminalStatus, render_event_jsonl,
};
use crate::workspace::{MaterializedWorkspace, WorkspaceError};

/// A failure while preparing an execution request inside the executor.
#[derive(Debug)]
pub enum PreparationError {
    InvalidRequest(ProtocolError),
    WorkspacePolicy(ConfigError),
    Workspace(WorkspaceError),
}

impl fmt::Display for PreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(error) => error.fmt(f),
            Self::WorkspacePolicy(error) => error.fmt(f),
            Self::Workspace(error) => error.fmt(f),
        }
    }
}

impl Error for PreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidRequest(error) => Some(error),
            Self::WorkspacePolicy(error) => Some(error),
            Self::Workspace(error) => Some(error),
        }
    }
}

/// Create the request's workspace in the executor's filesystem.
///
/// The path in `request.execution.workspace.parent` is interpreted by this
/// process. Assignment prompt, repository metadata, skills, repositories, and
/// launch environment all come from the request; this function has no daemon
/// or Tines client dependency.
pub fn prepare_workspace(
    request: &ExecutionRequest,
    on_git_output: impl FnMut(&str),
) -> Result<MaterializedWorkspace, PreparationError> {
    request
        .validate()
        .map_err(PreparationError::InvalidRequest)?;
    let api_url = Url::parse(&request.tines.api_url)
        .map_err(|_| PreparationError::InvalidRequest(ProtocolError::InvalidRequest))?;
    let workspace_parent =
        config::resolve_executor_workspace_parent(request.execution.workspace.parent.as_deref())
            .map_err(PreparationError::WorkspacePolicy)?;
    MaterializedWorkspace::create_with_git_log(
        &workspace_parent,
        &request.assignment,
        &api_url,
        on_git_output,
    )
    .map_err(PreparationError::Workspace)
}

/// Render a workspace-preparation failure as a terminal executor protocol event.
pub fn render_preparation_failure(
    request: &ExecutionRequest,
    error: &dyn fmt::Display,
) -> Result<String, ProtocolError> {
    let event = ExecutionEvent::new(ExecutionEventKind::Result {
        result: TerminalResult {
            status: TerminalStatus::Failed,
            exit_code: Some(1),
            error: Some(error.to_string()),
            provider_session_id: None,
            usage: None,
            pricing_evidence: None,
            interrupted: false,
            rate_limit: None,
        },
    });
    render_event_jsonl(&event, request)
}
