//! Assignment metadata used to select per-assignment runner configuration.

use std::error::Error;
use std::fmt;

use crate::config::{Config, ConfigResolution, MatchContext};
use crate::protocol::RunnerAssignment;
use crate::protocol::client::{Client, ClientError, RunLogBuffer};

/// A resolved assignment ready to send to the configured executor.
#[derive(Clone)]
pub struct PreparedAssignment {
    resolved: ResolvedAssignment,
    run_logs: RunLogBuffer,
}

impl PreparedAssignment {
    pub fn new(resolved: ResolvedAssignment) -> Self {
        let run_logs = RunLogBuffer::for_assignment(resolved.assignment());
        Self { resolved, run_logs }
    }

    /// Share the ordered run-log stream with the poll loop and executor worker.
    pub fn with_run_log_buffer(mut self, run_logs: RunLogBuffer) -> Self {
        self.run_logs = run_logs;
        self
    }

    pub fn resolved(&self) -> &ResolvedAssignment {
        &self.resolved
    }

    pub fn assignment(&self) -> &RunnerAssignment {
        self.resolved.assignment()
    }

    /// Return the shared log stream for execution and supervisor coordination.
    pub fn run_log_buffer(&self) -> RunLogBuffer {
        self.run_logs.clone()
    }

    pub fn log_delivery_cancelled(&self) -> bool {
        self.run_logs.is_cancelled()
    }

    /// Stop delivery when the supervisor has already canceled or settled the run.
    pub fn stop_log_delivery(&self) {
        self.run_logs.stop_sending();
    }
}

impl std::ops::Deref for PreparedAssignment {
    type Target = ResolvedAssignment;

    fn deref(&self) -> &Self::Target {
        &self.resolved
    }
}

impl fmt::Debug for PreparedAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedAssignment")
            .field("resolved", &self.resolved)
            .field("run_logs", &self.run_logs)
            .finish()
    }
}

/// Owned project, workflow, and state names for one assignment.
///
/// The state comes from the immutable run-start snapshot. The workflow comes
/// from the issue detail fetched when the assignment is resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssignmentMatchContext {
    project: String,
    workflow: String,
    state: String,
}

impl AssignmentMatchContext {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn workflow(&self) -> &str {
        &self.workflow
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    fn as_config_context(&self) -> MatchContext<'_> {
        MatchContext {
            project: &self.project,
            workflow: &self.workflow,
            state: &self.state,
        }
    }
}

/// The original protocol assignment, match metadata, and effective config.
#[derive(Clone, PartialEq)]
pub struct ResolvedAssignment {
    assignment: RunnerAssignment,
    context: AssignmentMatchContext,
    resolution: ConfigResolution,
    api_url: String,
}

impl ResolvedAssignment {
    /// The original protocol assignment, including its run payload.
    pub fn assignment(&self) -> &RunnerAssignment {
        &self.assignment
    }

    pub fn context(&self) -> &AssignmentMatchContext {
        &self.context
    }

    pub fn resolution(&self) -> &ConfigResolution {
        &self.resolution
    }

    /// Tines API URL carried into the local executor request.
    pub fn api_url(&self) -> &str {
        &self.api_url
    }
}

impl fmt::Debug for ResolvedAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedAssignment")
            .field("run_id", &self.assignment.run.id)
            .field("context", &self.context)
            .field("resolution", &self.resolution)
            .finish()
    }
}

impl Eq for ResolvedAssignment {}

/// An error while building match metadata or resolving assignment config.
#[derive(Debug)]
pub struct AssignmentResolutionError {
    run_id: String,
    cause: AssignmentResolutionCause,
}

#[derive(Debug)]
enum AssignmentResolutionCause {
    MissingMetadata(&'static str),
    IssueLookup(ClientError),
    MissingWorkflow,
}

impl fmt::Display for AssignmentResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "assignment {} cannot resolve runner configuration: ",
            self.run_id
        )?;
        match &self.cause {
            AssignmentResolutionCause::MissingMetadata(field) => {
                write!(f, "missing required {field}")
            }
            AssignmentResolutionCause::IssueLookup(error) => write!(
                f,
                "could not fetch issue detail for workflow metadata: {error}"
            ),
            AssignmentResolutionCause::MissingWorkflow => {
                f.write_str("issue detail did not include a workflow name")
            }
        }
    }
}

impl Error for AssignmentResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.cause {
            AssignmentResolutionCause::IssueLookup(error) => Some(error),
            _ => None,
        }
    }
}

/// Fetch required assignment metadata with the run key and resolve its config.
pub fn resolve_assignment(
    config: &Config,
    client: &Client,
    assignment: &RunnerAssignment,
) -> Result<ResolvedAssignment, AssignmentResolutionError> {
    let run_id = assignment.run.id.clone();
    let missing = |field| AssignmentResolutionError {
        run_id: run_id.clone(),
        cause: AssignmentResolutionCause::MissingMetadata(field),
    };

    let project = assignment
        .run
        .issue_ref
        .as_ref()
        .map(|reference| reference.project_name.trim())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| missing("project name in assignment.run.issue_ref.project_name"))?
        .to_owned();
    let state = assignment
        .run
        .state_at_start_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| missing("state name in assignment.run.state_at_start_name"))?
        .to_owned();
    let issue_id = assignment.run.issue_id.trim();
    if issue_id.is_empty() {
        return Err(missing("issue ID in assignment.run.issue_id"));
    }
    if assignment.run_key.trim().is_empty() {
        return Err(missing("assignment run key"));
    }

    let issue = client
        .get_issue(issue_id, &assignment.run_key)
        .map_err(|error| AssignmentResolutionError {
            run_id: run_id.clone(),
            cause: AssignmentResolutionCause::IssueLookup(error),
        })?;
    let workflow = issue
        .workflow
        .map(|workflow| workflow.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .ok_or(AssignmentResolutionError {
            run_id,
            cause: AssignmentResolutionCause::MissingWorkflow,
        })?;

    let context = AssignmentMatchContext {
        project,
        workflow,
        state,
    };
    let resolution = config.resolve_with_matches(context.as_config_context());
    Ok(ResolvedAssignment {
        assignment: assignment.clone(),
        context,
        resolution,
        api_url: config.server_url.to_string(),
    })
}
