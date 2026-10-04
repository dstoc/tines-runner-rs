//! Generic configured-command harness and plain-text event translation.

use std::process::Command;

use crate::effort::{EffortCapabilities, assignment_effort_rejection};
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRequest, LogStream, TerminalResult, TerminalStatus,
};
use crate::executor::harness::{
    HarnessAdapter, HarnessAdapterError, HarnessEventParser, HarnessExit, HarnessLaunch,
};
use crate::executor::workspace::MaterializedWorkspace;

const STDERR_DIAGNOSTIC_LIMIT: usize = 8 * 1024;

/// Adapter for an argv command selected by local runner configuration.
#[derive(Clone, Copy, Debug, Default)]
pub struct CustomAdapter;

impl HarnessAdapter for CustomAdapter {
    fn identifier(&self) -> &'static str {
        "custom"
    }

    fn launch(
        &self,
        request: &ExecutionRequest,
        workspace: &MaterializedWorkspace,
        capabilities: &EffortCapabilities,
    ) -> Result<HarnessLaunch, HarnessAdapterError> {
        if request.execution.harness != self.identifier() {
            return Err(HarnessAdapterError::new(
                "execution request does not match the custom adapter",
            ));
        }
        if assignment_effort_rejection(&request.assignment, capabilities).is_some() {
            return Err(HarnessAdapterError::new(
                "custom harness does not support Codex effort settings",
            ));
        }
        let configured =
            request.execution.custom_command.as_ref().ok_or_else(|| {
                HarnessAdapterError::new("custom harness command is not configured")
            })?;
        if configured.is_empty()
            || configured[0].trim().is_empty()
            || configured.iter().any(|argument| argument.contains('\0'))
        {
            return Err(HarnessAdapterError::new(
                "custom harness command is invalid",
            ));
        }

        let workspace_path = workspace
            .path()
            .canonicalize()
            .map_err(|_| HarnessAdapterError::new("could not resolve custom harness workspace"))?;
        let prompt_file = workspace_path.join("prompt.md");
        let argv = configured
            .iter()
            .map(|argument| {
                argument
                    .replace("{prompt_file}", &prompt_file.to_string_lossy())
                    .replace("{workspace}", &workspace_path.to_string_lossy())
            })
            .collect::<Vec<_>>();

        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(&workspace_path);
        workspace.environment().apply_to(&mut command);

        let secrets = diagnostic_secrets(workspace);
        let safe_argv = argv
            .iter()
            .map(|argument| redact_diagnostic_value(argument, &secrets))
            .collect::<Vec<_>>();
        let safe_workspace = redact_diagnostic_value(&workspace_path.to_string_lossy(), &secrets);
        let environment_names = workspace
            .environment()
            .variable_names()
            .map(|name| redact_diagnostic_value(name, &secrets))
            .chain(["TINES_API_KEY".to_owned(), "TINES_API_URL".to_owned()])
            .collect::<Vec<_>>();
        let diagnostics = format!(
            "$ {safe_argv:?}\n# tines runner: version={} harness=custom cwd=workspace workspace={safe_workspace:?} environment_names={environment_names:?}\n",
            crate::VERSION,
        );
        Ok(HarnessLaunch::new(command, diagnostics))
    }

    fn event_parser(&self) -> Box<dyn HarnessEventParser> {
        Box::new(CustomEventParser::new(Vec::new()))
    }

    fn event_parser_for_request(&self, request: &ExecutionRequest) -> Box<dyn HarnessEventParser> {
        Box::new(CustomEventParser::new(request.secret_patterns()))
    }
}

struct CustomEventParser {
    stderr_diagnostic: String,
    stderr_pending: String,
    secrets: Vec<String>,
    redaction_window: usize,
}

impl CustomEventParser {
    fn new(secrets: Vec<String>) -> Self {
        let redaction_window = secrets
            .iter()
            .map(|secret| secret.chars().count())
            .max()
            .unwrap_or_default()
            .saturating_sub(1);
        Self {
            stderr_diagnostic: String::new(),
            stderr_pending: String::new(),
            secrets,
            redaction_window,
        }
    }

    fn redact_stderr(&self, value: &str) -> String {
        self.secrets
            .iter()
            .fold(value.to_owned(), |redacted, secret| {
                redacted.replace(secret, "***")
            })
    }

    fn take_safe_stderr_prefix(&mut self, chunk: &str, finish: bool) -> String {
        self.stderr_pending.push_str(chunk);
        let characters = self.stderr_pending.chars().collect::<Vec<_>>();
        let safe_starts = if finish {
            characters.len()
        } else {
            characters.len().saturating_sub(self.redaction_window)
        };
        let offsets = self
            .stderr_pending
            .char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(self.stderr_pending.len()))
            .collect::<Vec<_>>();
        let mut output = String::new();
        let mut index = 0;
        while index < safe_starts {
            let byte_index = offsets[index];
            if let Some(secret) = self
                .secrets
                .iter()
                .find(|secret| self.stderr_pending[byte_index..].starts_with(secret.as_str()))
            {
                output.push_str("***");
                index += secret.chars().count();
            } else {
                output.push(characters[index]);
                index += 1;
            }
        }
        self.stderr_pending = characters[index..].iter().collect();
        output
    }

    fn flush_stderr_diagnostic(&mut self) {
        let final_chunk = self.take_safe_stderr_prefix("", true);
        append_stderr_tail(&mut self.stderr_diagnostic, &final_chunk);
    }

    fn finish_stderr_output(&mut self) -> Vec<ExecutionEvent> {
        let final_chunk = self.take_safe_stderr_prefix("", true);
        append_stderr_tail(&mut self.stderr_diagnostic, &final_chunk);
        log_event(LogStream::Stderr, &final_chunk)
    }
}

impl HarnessEventParser for CustomEventParser {
    fn push(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        log_event(LogStream::Stdout, chunk)
    }

    fn push_stderr(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        if chunk.is_empty() {
            return Vec::new();
        }
        let safe_diagnostic = self.take_safe_stderr_prefix(chunk, false);
        append_stderr_tail(&mut self.stderr_diagnostic, &safe_diagnostic);
        log_event(LogStream::Stderr, &safe_diagnostic)
    }

    fn finish(&mut self) -> Vec<ExecutionEvent> {
        Vec::new()
    }

    fn finish_stderr(&mut self) -> Vec<ExecutionEvent> {
        self.finish_stderr_output()
    }

    fn terminal_result(&mut self, exit: HarnessExit) -> ExecutionEvent {
        self.flush_stderr_diagnostic();
        let completed = !exit.interrupted && exit.exit_code == Some(0) && exit.error.is_none();
        let status = if completed {
            TerminalStatus::Completed
        } else {
            TerminalStatus::Failed
        };
        let mut error = if completed {
            None
        } else {
            let base = exit.error.unwrap_or_else(|| {
                if exit.interrupted {
                    "custom harness execution was interrupted".to_owned()
                } else if let Some(code) = exit.exit_code {
                    format!("custom harness exited with code {code}")
                } else {
                    "custom harness exited without a status".to_owned()
                }
            });
            if self.stderr_diagnostic.is_empty() {
                Some(base)
            } else {
                Some(format!(
                    "{base}\ncustom harness stderr:\n{}",
                    self.stderr_diagnostic
                ))
            }
        };
        if let Some(error) = &mut error {
            *error = self.redact_stderr(error);
        }

        ExecutionEvent::new(ExecutionEventKind::Result {
            result: TerminalResult {
                status,
                exit_code: exit.exit_code,
                error,
                provider_session_id: None,
                usage: None,
                pricing_evidence: None,
                interrupted: exit.interrupted,
                rate_limit: None,
            },
        })
    }
}

fn log_event(stream: LogStream, message: &str) -> Vec<ExecutionEvent> {
    if message.is_empty() {
        return Vec::new();
    }
    vec![ExecutionEvent::new(ExecutionEventKind::Log {
        stream,
        message: message.to_owned(),
    })]
}

fn append_stderr_tail(buffer: &mut String, chunk: &str) {
    buffer.push_str(chunk);
    let character_count = buffer.chars().count();
    if character_count > STDERR_DIAGNOSTIC_LIMIT {
        let skip = character_count - STDERR_DIAGNOSTIC_LIMIT;
        *buffer = buffer.chars().skip(skip).collect();
        let marker = "[earlier stderr omitted]\n";
        if !buffer.starts_with(marker) {
            let tail = buffer.clone();
            *buffer = format!("{marker}{tail}");
        }
    }
}

fn diagnostic_secrets(workspace: &MaterializedWorkspace) -> Vec<String> {
    let mut secrets = workspace
        .environment()
        .secret_values()
        .filter(|secret| !secret.is_empty())
        .flat_map(|secret| {
            let json_escaped = serde_json::to_string(secret).expect("Rust strings serialize");
            let rust_escaped = format!("{secret:?}");
            [
                secret.to_owned(),
                json_escaped[1..json_escaped.len() - 1].to_owned(),
                rust_escaped[1..rust_escaped.len() - 1].to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets
}

fn redact_diagnostic_value(value: &str, secrets: &[String]) -> String {
    secrets.iter().fold(value.to_owned(), |redacted, secret| {
        redacted.replace(secret, "[REDACTED]")
    })
}
