//! Executor-side harness selection and normalized event interfaces.

use std::fmt;
use std::process::Command;

use crate::effort::EffortCapabilities;
use crate::execution_protocol::{ExecutionEvent, ExecutionEventKind, ExecutionRequest};
use crate::executor::workspace::MaterializedWorkspace;

/// Select an adapter from the semantic harness identifier in an execution
/// request. The identifier never contains a machine-specific executable path.
pub fn adapter_for(identifier: &str) -> Result<Box<dyn HarnessAdapter>, UnsupportedHarness> {
    match identifier {
        "codex" => Ok(Box::new(crate::executor::codex_adapter::CodexAdapter)),
        "custom" => Ok(Box::new(crate::executor::custom_adapter::CustomAdapter)),
        _ => Err(UnsupportedHarness),
    }
}

/// Common executor interface implemented by each supported harness adapter.
pub trait HarnessAdapter: Send + Sync {
    fn identifier(&self) -> &'static str;

    /// Build a launch command inside the executor environment.
    fn launch(
        &self,
        request: &ExecutionRequest,
        workspace: &MaterializedWorkspace,
        capabilities: &EffortCapabilities,
    ) -> Result<HarnessLaunch, HarnessAdapterError>;

    /// Create a parser that translates native output into protocol events.
    fn event_parser(&self) -> Box<dyn HarnessEventParser>;

    /// Create a parser with assignment context when secret-aware streaming is needed.
    fn event_parser_for_request(&self, _request: &ExecutionRequest) -> Box<dyn HarnessEventParser> {
        self.event_parser()
    }
}

/// A directly spawned process and safe diagnostic text for the launch.
pub struct HarnessLaunch {
    command: Command,
    diagnostics: String,
}

impl HarnessLaunch {
    pub(crate) fn new(command: Command, diagnostics: String) -> Self {
        Self {
            command,
            diagnostics,
        }
    }

    pub fn command(&mut self) -> &mut Command {
        &mut self.command
    }

    pub fn into_command(self) -> Command {
        self.command
    }

    pub fn diagnostics(&self) -> &str {
        &self.diagnostics
    }
}

impl fmt::Debug for HarnessLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HarnessLaunch")
            .field("diagnostics", &self.diagnostics)
            .finish_non_exhaustive()
    }
}

/// Normalized process outcome passed to the adapter for terminal conversion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessExit {
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub interrupted: bool,
}

/// Native stream parser that returns only versioned, harness-neutral events.
pub trait HarnessEventParser: Send {
    fn push(&mut self, chunk: &str) -> Vec<ExecutionEvent>;
    /// Translate stderr while retaining any bounded diagnostic context needed
    /// to describe a failed process.
    fn push_stderr(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        if chunk.is_empty() {
            return Vec::new();
        }
        vec![ExecutionEvent::new(ExecutionEventKind::Log {
            stream: crate::execution_protocol::LogStream::Stderr,
            message: chunk.to_owned(),
        })]
    }
    fn finish(&mut self) -> Vec<ExecutionEvent>;
    /// Flush output held back for secret redaction across chunk boundaries.
    fn finish_stderr(&mut self) -> Vec<ExecutionEvent> {
        Vec::new()
    }
    fn terminal_result(&mut self, exit: HarnessExit) -> ExecutionEvent;
}

/// Safe error returned when the request names an unsupported harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedHarness;

impl fmt::Display for UnsupportedHarness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unsupported execution harness")
    }
}

impl std::error::Error for UnsupportedHarness {}

/// Failure while constructing an adapter launch command.
#[derive(Debug)]
pub struct HarnessAdapterError {
    message: &'static str,
}

impl HarnessAdapterError {
    pub(crate) fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for HarnessAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for HarnessAdapterError {}
