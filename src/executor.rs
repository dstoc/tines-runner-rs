//! Executor-side lifecycle for one self-contained assignment request.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use url::Url;

use crate::config::{self, ConfigError, WorkspaceRetention};
use crate::effort::EffortCapabilities;
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRequest, LogStream, ProtocolError, TerminalResult,
    TerminalStatus, render_event_jsonl,
};
use crate::executor::harness::{HarnessExit, adapter_for};
use crate::executor::workspace::{MaterializedWorkspace, WorkspaceError};
use crate::process::{
    PROCESS_TREE_TERMINATION_GRACE, ProcessExit, ProcessStream, SupervisedProcess,
};
use crate::protocol::FinishStatus;
use crate::retention;
use crate::shutdown::ShutdownSignal;

pub mod codex;
pub mod codex_adapter;
pub mod codex_stream;
pub mod harness;
pub mod workspace;

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
        config::resolve_workspace_parent(request.execution.workspace.parent.as_deref())
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

/// Execute one assignment without using the Tines runner protocol.
///
/// For a valid request, this writes generic executor events to `output` and
/// emits one terminal result after workspace preparation, harness supervision,
/// and local retention settlement finish.
pub fn execute_request(
    request: &ExecutionRequest,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    shutdown: &ShutdownSignal,
) -> ExitCode {
    if let Err(error) = request.validate() {
        let _ = writeln!(diagnostics, "executor request rejected: {error}");
        return ExitCode::FAILURE;
    }

    let events = match adapter_for(&request.execution.harness) {
        Ok(adapter) => adapter,
        Err(error) => {
            return emit_failure(request, output, diagnostics, error.to_string());
        }
    };
    let mut parser = events.event_parser();
    let retention = retention_policy(request);
    let workspace_parent =
        match config::resolve_workspace_parent(request.execution.workspace.parent.as_deref()) {
            Ok(parent) => parent,
            Err(error) => {
                return emit_failure(
                    request,
                    output,
                    diagnostics,
                    format!("could not resolve executor workspace parent: {error}"),
                );
            }
        };
    if retention::prune_retained(&workspace_parent, &retention).is_err() {
        return emit_failure(
            request,
            output,
            diagnostics,
            "could not prune retained workspaces".to_owned(),
        );
    }

    let mut output_error: Option<String> = None;
    let workspace = match prepare_workspace(request, |message| {
        if output_error.is_none() {
            output_error = write_event(
                request,
                output,
                &ExecutionEvent::new(ExecutionEventKind::Log {
                    stream: LogStream::System,
                    message: message.to_owned(),
                }),
            )
            .err()
            .map(|error| error.to_string());
        }
    }) {
        Ok(workspace) => workspace,
        Err(error) => {
            let terminal = parser.terminal_result(HarnessExit {
                exit_code: Some(1),
                error: Some(error.to_string()),
                interrupted: false,
            });
            return emit_terminal(request, output, diagnostics, terminal, None, &retention);
        }
    };

    if output_error.is_none() {
        let capabilities = if request.assignment.effort.is_some() {
            EffortCapabilities::discover(crate::VERSION)
        } else {
            EffortCapabilities {
                version: 1,
                daemon_version: crate::VERSION.to_owned(),
                harness: request.execution.harness.clone(),
                harness_version: "not required".to_owned(),
                catalog_digest: String::new(),
                models: Vec::new(),
                accepts_asserted_effort: None,
                discovery_error: None,
            }
        };
        match events.launch(request, &workspace, &capabilities) {
            Ok(mut launch) => {
                let diagnostic = launch.diagnostics().to_owned();
                output_error = write_event(
                    request,
                    output,
                    &ExecutionEvent::new(ExecutionEventKind::Log {
                        stream: LogStream::System,
                        message: diagnostic,
                    }),
                )
                .err()
                .map(|error| error.to_string());
                if output_error.is_none() {
                    let command = launch.command();
                    match SupervisedProcess::spawn_with_output(command) {
                        Ok(process) => {
                            let stdout = Utf8StreamDecoder::default();
                            let stderr = Utf8StreamDecoder::default();
                            let mut stdout_decoder = stdout;
                            let mut stderr_decoder = stderr;
                            let mut streamed_error: Option<String> = None;
                            let timeout = Duration::from_secs(
                                request.assignment.timeout_minutes.saturating_mul(60),
                            );
                            match process.wait_timeout_with_output_or_shutdown(
                                timeout,
                                PROCESS_TREE_TERMINATION_GRACE,
                                || false,
                                || shutdown.is_requested(),
                                |chunk| {
                                    if streamed_error.is_some() {
                                        return;
                                    }
                                    match chunk.stream {
                                        ProcessStream::Stdout => {
                                            let text = stdout_decoder.push(&chunk.bytes);
                                            for event in parser.push(&text) {
                                                if let Err(error) =
                                                    write_event(request, output, &event)
                                                {
                                                    streamed_error = Some(error.to_string());
                                                    break;
                                                }
                                            }
                                        }
                                        ProcessStream::Stderr => {
                                            let message = stderr_decoder.push(&chunk.bytes);
                                            if !message.is_empty() {
                                                streamed_error = write_event(
                                                    request,
                                                    output,
                                                    &ExecutionEvent::new(ExecutionEventKind::Log {
                                                        stream: LogStream::Stderr,
                                                        message,
                                                    }),
                                                )
                                                .err()
                                                .map(|error| error.to_string());
                                            }
                                        }
                                    }
                                },
                                || {},
                            ) {
                                Ok(output_status) => {
                                    if streamed_error.is_none() {
                                        let stdout_tail = stdout_decoder.finish();
                                        for event in parser.push(&stdout_tail) {
                                            if let Err(error) = write_event(request, output, &event)
                                            {
                                                streamed_error = Some(error.to_string());
                                                break;
                                            }
                                        }
                                        if streamed_error.is_none() {
                                            for event in parser.finish() {
                                                if let Err(error) =
                                                    write_event(request, output, &event)
                                                {
                                                    streamed_error = Some(error.to_string());
                                                    break;
                                                }
                                            }
                                        }
                                        let stderr_tail = stderr_decoder.finish();
                                        if streamed_error.is_none() && !stderr_tail.is_empty() {
                                            streamed_error = write_event(
                                                request,
                                                output,
                                                &ExecutionEvent::new(ExecutionEventKind::Log {
                                                    stream: LogStream::Stderr,
                                                    message: stderr_tail,
                                                }),
                                            )
                                            .err()
                                            .map(|error| error.to_string());
                                        }
                                    }

                                    if output_status.interrupted {
                                        if workspace.cleanup().is_err() {
                                            let _ = writeln!(
                                                diagnostics,
                                                "executor could not clean workspace after interruption"
                                            );
                                        }
                                        return ExitCode::FAILURE;
                                    }

                                    let output_failed = streamed_error.is_some();
                                    let error =
                                        streamed_error.or_else(|| {
                                            output_status.timed_out.then(|| format!(
                                            "harness exceeded the {}-minute assignment timeout",
                                            request.assignment.timeout_minutes
                                        ))
                                        });
                                    let exit_code = if output_status.timed_out || output_failed {
                                        Some(1)
                                    } else {
                                        match output_status.exit {
                                            ProcessExit::Code(code) => Some(code),
                                            ProcessExit::Signal(_) | ProcessExit::Unknown => None,
                                        }
                                    };
                                    let terminal = parser.terminal_result(HarnessExit {
                                        exit_code,
                                        error,
                                        interrupted: false,
                                    });
                                    return emit_terminal(
                                        request,
                                        output,
                                        diagnostics,
                                        terminal,
                                        Some(&workspace),
                                        &retention,
                                    );
                                }
                                Err(error) => {
                                    output_error =
                                        Some(format!("could not supervise harness: {error}"));
                                }
                            }
                        }
                        Err(error) => {
                            output_error = Some(format!("could not start harness: {error}"));
                        }
                    }
                }
            }
            Err(error) => output_error = Some(error.to_string()),
        }
    }

    let terminal = parser.terminal_result(HarnessExit {
        exit_code: Some(1),
        error: output_error.or_else(|| Some("harness execution failed".to_owned())),
        interrupted: false,
    });
    emit_terminal(
        request,
        output,
        diagnostics,
        terminal,
        Some(&workspace),
        &retention,
    )
}

fn retention_policy(request: &ExecutionRequest) -> WorkspaceRetention {
    WorkspaceRetention {
        mode: request.execution.retention.mode,
        max_age: Duration::from_secs(
            request
                .execution
                .retention
                .max_age_hours
                .saturating_mul(60 * 60),
        ),
        max_count: request.execution.retention.max_count,
    }
}

fn emit_failure(
    request: &ExecutionRequest,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    error: String,
) -> ExitCode {
    let event = ExecutionEvent::new(ExecutionEventKind::Result {
        result: TerminalResult {
            status: TerminalStatus::Failed,
            exit_code: Some(1),
            error: Some(error),
            provider_session_id: None,
            usage: None,
            pricing_evidence: None,
            interrupted: false,
            rate_limit: None,
        },
    });
    match write_event(request, output, &event).and_then(|()| output.flush()) {
        Ok(()) => ExitCode::FAILURE,
        Err(_) => {
            let _ = writeln!(diagnostics, "executor could not write its failure result");
            ExitCode::FAILURE
        }
    }
}

fn emit_terminal(
    request: &ExecutionRequest,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    mut event: ExecutionEvent,
    workspace: Option<&MaterializedWorkspace>,
    retention: &WorkspaceRetention,
) -> ExitCode {
    let Some(workspace) = workspace else {
        return match write_event(request, output, &event).and_then(|()| output.flush()) {
            Ok(()) => ExitCode::FAILURE,
            Err(_) => {
                let _ = writeln!(diagnostics, "executor could not write its terminal result");
                ExitCode::FAILURE
            }
        };
    };

    let terminal_status = {
        let terminal = match &mut event.kind {
            ExecutionEventKind::Result { result } => result,
            _ => unreachable!("executor terminal event must be a result"),
        };
        let status = match terminal.status {
            TerminalStatus::Completed => FinishStatus::Completed,
            TerminalStatus::Failed | TerminalStatus::RateLimited => FinishStatus::Failed,
        };
        let issue_ref = request
            .assignment
            .run
            .issue_ref
            .as_ref()
            .map(|issue| format!("{}/{}", issue.project_name, issue.number));
        if retention::settle_workspace(
            workspace.path(),
            retention,
            &request.assignment.run.id,
            issue_ref,
            status,
            terminal.error.as_deref(),
        )
        .is_err()
        {
            if terminal.status == TerminalStatus::Completed {
                terminal.status = TerminalStatus::Failed;
            }
            if terminal.exit_code == Some(0) || terminal.exit_code.is_none() {
                terminal.exit_code = Some(1);
            }
            terminal.error = Some(match terminal.error.take() {
                Some(error) => format!("{error}; workspace settlement failed"),
                None => "workspace settlement failed".to_owned(),
            });
        }
        terminal.status
    };

    if let Err(error) = write_event(request, output, &event).and_then(|()| output.flush()) {
        let _ = writeln!(
            diagnostics,
            "executor could not write its terminal result: {error}"
        );
        return ExitCode::FAILURE;
    }

    if terminal_status == TerminalStatus::Completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn write_event(
    request: &ExecutionRequest,
    output: &mut impl Write,
    event: &ExecutionEvent,
) -> io::Result<()> {
    let line = render_event_jsonl(event, request)
        .map_err(|_| io::Error::other("could not encode executor event"))?;
    output.write_all(line.as_bytes())
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
