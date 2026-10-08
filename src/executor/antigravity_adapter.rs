//! Antigravity adapter and native stream-to-executor event translation.

use crate::effort::EffortCapabilities;
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRequest, ExecutionUsage, LogStream,
    TerminalResult, TerminalStatus,
};
use crate::executor::antigravity::AntigravityLaunch;
use crate::executor::antigravity_stream::{AntigravityEvent, AntigravityStreamParser};
use crate::executor::harness::{
    HarnessAdapter, HarnessAdapterError, HarnessEventParser, HarnessExit, HarnessLaunch,
};
use crate::executor::workspace::MaterializedWorkspace;
use serde_json::Value;

const STDERR_DIAGNOSTIC_LIMIT: usize = 8 * 1024;
const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;

/// Dedicated adapter for Google's Antigravity CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct AntigravityAdapter;

impl HarnessAdapter for AntigravityAdapter {
    fn identifier(&self) -> &'static str {
        "antigravity"
    }

    fn launch(
        &self,
        request: &ExecutionRequest,
        workspace: &MaterializedWorkspace,
        _capabilities: &EffortCapabilities,
    ) -> Result<HarnessLaunch, HarnessAdapterError> {
        let launch = AntigravityLaunch::for_execution_request(request, workspace)
            .map_err(HarnessAdapterError::new)?;
        let diagnostics = launch.diagnostics(request);
        let stdin = launch.stdin().to_vec();
        Ok(HarnessLaunch::new_with_stdin(
            launch.command(workspace),
            stdin,
            diagnostics,
        ))
    }

    fn event_parser(&self) -> Box<dyn HarnessEventParser> {
        Box::<AntigravityEventParser>::default()
    }

    fn event_parser_for_request(&self, request: &ExecutionRequest) -> Box<dyn HarnessEventParser> {
        Box::new(AntigravityEventParser::with_secrets(
            request.secret_patterns(),
        ))
    }
}

#[derive(Default)]
struct AntigravityEventParser {
    parser: AntigravityStreamParser,
    conversation_id: Option<String>,
    terminal: Option<AntigravityTerminal>,
    usage: Option<ExecutionUsage>,
    malformed: bool,
    response_streamed: bool,
    stderr_pending: String,
    stderr_diagnostic: String,
    stderr_secrets: Vec<String>,
    stderr_redaction_window: usize,
}

#[derive(Clone, Debug)]
struct AntigravityTerminal {
    status: String,
    error: Option<String>,
    denied_actions: bool,
}

impl AntigravityEventParser {
    fn with_secrets(stderr_secrets: Vec<String>) -> Self {
        let stderr_redaction_window = stderr_secrets
            .iter()
            .map(|secret| secret.chars().count())
            .max()
            .unwrap_or_default()
            .saturating_sub(1);
        Self {
            stderr_secrets,
            stderr_redaction_window,
            ..Self::default()
        }
    }

    fn translate(&mut self, event: AntigravityEvent) -> Vec<ExecutionEvent> {
        match event {
            AntigravityEvent::Init(raw) => {
                let mut events = Vec::new();
                if let Some(id) = raw.get("conversation_id").and_then(Value::as_str) {
                    self.record_session(id, &mut events);
                }
                events
            }
            AntigravityEvent::StepUpdate(raw) => {
                let Some(step) = raw.get("step_update") else {
                    return self.malformed_event();
                };
                let mut events = Vec::new();
                if let Some(id) = step.get("conversation_id").and_then(Value::as_str) {
                    self.record_session(id, &mut events);
                }
                match step.get("step_type").and_then(Value::as_str) {
                    Some("agent_response") => {
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str)
                            && !delta.is_empty()
                        {
                            self.response_streamed = true;
                            events.extend(log_text(LogStream::Stdout, delta));
                        }
                    }
                    Some("tool") => {
                        let name = step
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .or_else(|| step.pointer("/tool_info/name").and_then(Value::as_str));
                        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
                            let state = step.get("state").and_then(Value::as_str);
                            let message = state.map_or_else(
                                || format!("[tool] {}", clip(name, 160)),
                                |state| format!("[tool] {} ({})", clip(name, 160), clip(state, 32)),
                            );
                            events.push(log_event(LogStream::Stdout, message));
                        }
                    }
                    _ => {}
                }
                events
            }
            AntigravityEvent::Result(raw) => {
                let Some(result) = raw.get("result") else {
                    return self.malformed_event();
                };
                if self.terminal.is_some() {
                    return self.malformed_event();
                }
                let mut events = Vec::new();
                if let Some(id) = result.get("conversation_id").and_then(Value::as_str) {
                    self.record_session(id, &mut events);
                }
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let error = result
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|error| !error.trim().is_empty())
                    .map(|error| clip(error, 2_000));
                let denied_actions = result.get("denied_actions").is_some_and(is_nonempty_value);
                if let Some(usage) = result.get("usage").and_then(parse_usage) {
                    events.push(ExecutionEvent::new(ExecutionEventKind::Usage {
                        usage: usage.clone(),
                    }));
                    self.usage = Some(usage);
                }
                if let Some(error) = error.as_deref() {
                    events.push(ExecutionEvent::new(ExecutionEventKind::ProviderError {
                        provider: "antigravity".to_owned(),
                        code: None,
                        message: error.to_owned(),
                    }));
                }
                if !self.response_streamed
                    && let Some(response) = result.get("response").and_then(Value::as_str)
                {
                    events.extend(log_text(LogStream::Stdout, response));
                }
                self.terminal = Some(AntigravityTerminal {
                    status,
                    error,
                    denied_actions,
                });
                events
            }
            AntigravityEvent::Malformed => self.malformed_event(),
            AntigravityEvent::Other => Vec::new(),
        }
    }

    fn record_session(&mut self, id: &str, events: &mut Vec<ExecutionEvent>) {
        if id.trim().is_empty() || self.conversation_id.is_some() {
            return;
        }
        self.conversation_id = Some(id.to_owned());
        events.push(ExecutionEvent::new(ExecutionEventKind::Session {
            provider: "antigravity".to_owned(),
            id: id.to_owned(),
        }));
    }

    fn malformed_event(&mut self) -> Vec<ExecutionEvent> {
        self.malformed = true;
        vec![log_event(
            LogStream::System,
            "Antigravity emitted malformed stream JSON".to_owned(),
        )]
    }

    fn take_safe_stderr_prefix(&mut self, chunk: &str, finish: bool) -> String {
        self.stderr_pending.push_str(chunk);
        let characters = self.stderr_pending.chars().collect::<Vec<_>>();
        let safe_starts = if finish {
            characters.len()
        } else {
            characters
                .len()
                .saturating_sub(self.stderr_redaction_window)
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
                .stderr_secrets
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

    fn retain_stderr(&mut self, message: &str) {
        self.stderr_diagnostic.push_str(message);
        if self.stderr_diagnostic.chars().count() > STDERR_DIAGNOSTIC_LIMIT {
            let skip = self.stderr_diagnostic.chars().count() - STDERR_DIAGNOSTIC_LIMIT;
            self.stderr_diagnostic = self.stderr_diagnostic.chars().skip(skip).collect();
        }
    }
}

impl HarnessEventParser for AntigravityEventParser {
    fn push(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        self.parser
            .push(chunk)
            .into_iter()
            .flat_map(|event| self.translate(event))
            .collect()
    }

    fn finish(&mut self) -> Vec<ExecutionEvent> {
        self.parser
            .finish()
            .into_iter()
            .flat_map(|event| self.translate(event))
            .collect()
    }

    fn push_stderr(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix(chunk, false);
        self.retain_stderr(&message);
        log_stderr(&message)
    }

    fn finish_stderr(&mut self) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix("", true);
        self.retain_stderr(&message);
        log_stderr(&message)
    }

    fn terminal_result(&mut self, exit: HarnessExit) -> ExecutionEvent {
        let stderr_tail = self.take_safe_stderr_prefix("", true);
        self.retain_stderr(&stderr_tail);
        let terminal = self.terminal.clone();
        let provider_interrupted = terminal
            .as_ref()
            .is_some_and(|result| matches!(result.status.as_str(), "CANCELED" | "INTERRUPTED"));
        let interrupted = exit.interrupted || provider_interrupted;
        let success = !interrupted
            && !self.malformed
            && exit.exit_code == Some(0)
            && exit.error.is_none()
            && terminal
                .as_ref()
                .is_some_and(|result| result.status == "SUCCESS" && !result.denied_actions);
        let status = if success {
            TerminalStatus::Completed
        } else {
            TerminalStatus::Failed
        };
        let error = if success {
            None
        } else {
            let base = if interrupted {
                Some("Antigravity execution was interrupted".to_owned())
            } else if self.malformed {
                Some("Antigravity emitted malformed stream JSON".to_owned())
            } else if terminal.is_none() {
                exit.error.clone().or_else(|| {
                    Some("Antigravity stream ended without a terminal result".to_owned())
                })
            } else if terminal
                .as_ref()
                .is_some_and(|result| result.denied_actions)
            {
                Some("Antigravity reported denied actions".to_owned())
            } else if terminal
                .as_ref()
                .is_some_and(|result| result.status != "SUCCESS")
            {
                terminal
                    .as_ref()
                    .and_then(|result| result.error.clone())
                    .or_else(|| {
                        terminal
                            .as_ref()
                            .map(|result| format!("Antigravity returned status {}", result.status))
                    })
            } else {
                exit.error.or_else(|| {
                    exit.exit_code
                        .filter(|code| *code != 0)
                        .map(|code| format!("agy exited with code {code}"))
                })
            };
            let mut message = base.unwrap_or_else(|| "Antigravity execution failed".to_owned());
            if !self.stderr_diagnostic.is_empty() {
                message.push_str("\nAntigravity stderr:\n");
                message.push_str(&self.stderr_diagnostic);
            }
            Some(message)
        };

        ExecutionEvent::new(ExecutionEventKind::Result {
            result: TerminalResult {
                status,
                exit_code: (!interrupted).then_some(exit.exit_code).flatten(),
                error,
                provider_session_id: self.conversation_id.clone(),
                usage: self.usage.clone(),
                pricing_evidence: None,
                interrupted,
                rate_limit: None,
            },
        })
    }
}

fn parse_usage(value: &Value) -> Option<ExecutionUsage> {
    let metric = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value <= MAX_SAFE_JSON_INTEGER)
    };
    let usage = ExecutionUsage {
        input_tokens: metric("input_tokens"),
        output_tokens: metric("output_tokens"),
        cache_read_tokens: metric("cache_read_tokens"),
        cache_write_tokens: None,
    };
    (usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cache_read_tokens.is_some())
    .then_some(usage)
}

fn is_nonempty_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Bool(true) | Value::Number(_) => true,
    }
}

fn log_text(stream: LogStream, text: &str) -> Vec<ExecutionEvent> {
    const CHUNK_CHARS: usize = 32 * 1024;
    text.chars()
        .collect::<Vec<_>>()
        .chunks(CHUNK_CHARS)
        .map(|chunk| log_event(stream, chunk.iter().collect()))
        .collect()
}

fn log_event(stream: LogStream, message: String) -> ExecutionEvent {
    ExecutionEvent::new(ExecutionEventKind::Log { stream, message })
}

fn log_stderr(message: &str) -> Vec<ExecutionEvent> {
    if message.is_empty() {
        Vec::new()
    } else {
        vec![log_event(LogStream::Stderr, message.to_owned())]
    }
}

fn clip(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{AntigravityEventParser, HarnessEventParser, HarnessExit, STDERR_DIAGNOSTIC_LIMIT};
    use crate::execution_protocol::{ExecutionEventKind, LogStream, TerminalStatus};

    fn feed(
        parser: &mut AntigravityEventParser,
        stream: &str,
    ) -> Vec<crate::execution_protocol::ExecutionEvent> {
        let mut events = parser.push(stream);
        events.extend(parser.finish());
        events
    }

    fn terminal(
        parser: &mut AntigravityEventParser,
        exit_code: i32,
    ) -> crate::execution_protocol::ExecutionEvent {
        parser.terminal_result(HarnessExit {
            exit_code: Some(exit_code),
            error: None,
            interrupted: false,
        })
    }

    #[test]
    fn streams_response_once_and_preserves_session_and_usage() {
        let mut parser = AntigravityEventParser::default();
        let events = feed(
            &mut parser,
            "{\"event\":\"init\",\"conversation_id\":\"session-1\",\"init\":{}}\n{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"session-1\",\"step_type\":\"agent_response\",\"text_delta\":\"Hello\"}}\n{\"event\":\"result\",\"result\":{\"conversation_id\":\"session-1\",\"status\":\"SUCCESS\",\"response\":\"Hello\",\"usage\":{\"input_tokens\":8,\"output_tokens\":3,\"cache_read_tokens\":2,\"thinking_tokens\":99,\"total_tokens\":11}}}\n",
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, ExecutionEventKind::Session { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match &event.kind {
                    ExecutionEventKind::Log {
                        stream: LogStream::Stdout,
                        message,
                    } => Some(message.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["Hello"]
        );
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ExecutionEventKind::Usage { usage }
                if usage.input_tokens == Some(8)
                    && usage.output_tokens == Some(3)
                    && usage.cache_read_tokens == Some(2)
        )));
        let result = terminal(&mut parser, 0);
        assert!(matches!(
            result.kind,
            ExecutionEventKind::Result { result }
                if result.status == TerminalStatus::Completed
                    && result.provider_session_id.as_deref() == Some("session-1")
        ));
    }

    #[test]
    fn renders_concise_tool_progress_and_uses_terminal_response_without_deltas() {
        let mut parser = AntigravityEventParser::default();
        let events = feed(
            &mut parser,
            "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"tool\",\"tool_name\":\"run_command\",\"state\":\"ACTIVE\",\"tool_info\":{\"parameters\":{\"secret\":\"not logged\"}}}}\n{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"done\"}}\n",
        );
        let logs = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Log { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(logs, ["[tool] run_command (ACTIVE)", "done"]);
        let event = terminal(&mut parser, 0);
        let ExecutionEventKind::Result { result } = event.kind else {
            panic!("terminal result event expected");
        };
        assert_eq!(result.status, TerminalStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.error.is_none());
        assert!(result.provider_session_id.is_none());
    }

    #[test]
    fn every_non_success_status_and_denied_actions_fail() {
        for status in [
            "ERROR",
            "INVALID",
            "WAITING",
            "RUNNING",
            "CANCELED",
            "INTERRUPTED",
            "FUTURE",
        ] {
            let mut parser = AntigravityEventParser::default();
            let stream =
                format!("{{\"event\":\"result\",\"result\":{{\"status\":\"{status}\"}}}}\n");
            feed(&mut parser, &stream);
            let result = terminal(&mut parser, 0);
            assert!(
                matches!(result.kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed)
            );
        }
        let mut parser = AntigravityEventParser::default();
        feed(
            &mut parser,
            "{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"denied_actions\":[{\"tool\":\"write\"}]}}\n",
        );
        assert!(
            matches!(terminal(&mut parser, 0).kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed && result.error.as_deref().is_some_and(|error| error.contains("denied actions")))
        );
    }

    #[test]
    fn malformed_or_missing_results_never_complete() {
        let mut parser = AntigravityEventParser::default();
        feed(
            &mut parser,
            "not-json\n{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\n",
        );
        assert!(
            matches!(terminal(&mut parser, 0).kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed)
        );

        let mut parser = AntigravityEventParser::default();
        feed(&mut parser, "");
        assert!(
            matches!(terminal(&mut parser, 0).kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed && result.error.as_deref().is_some_and(|error| error.contains("without a terminal result")))
        );
    }

    #[test]
    fn process_exit_and_interruption_are_required_for_success() {
        let mut parser = AntigravityEventParser::default();
        feed(
            &mut parser,
            "{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\n",
        );
        assert!(
            matches!(terminal(&mut parser, 1).kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed)
        );

        let mut parser = AntigravityEventParser::default();
        feed(
            &mut parser,
            "{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\n",
        );
        let event = parser.terminal_result(HarnessExit {
            exit_code: Some(0),
            error: None,
            interrupted: true,
        });
        assert!(
            matches!(event.kind, ExecutionEventKind::Result { result } if result.status == TerminalStatus::Failed && result.interrupted)
        );
    }

    #[test]
    fn stderr_redaction_spans_chunks_and_bounds_failure_context() {
        let mut parser = AntigravityEventParser::with_secrets(vec!["secret-value".to_owned()]);
        let mut events = parser.push_stderr("prefix secret-");
        events.extend(parser.push_stderr("value suffix\n"));
        events.extend(parser.finish_stderr());
        let rendered = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Log {
                    stream: LogStream::Stderr,
                    message,
                } => Some(message.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(rendered, "prefix *** suffix\n");
        let result = terminal(&mut parser, 1);
        if let ExecutionEventKind::Result { result } = result.kind {
            let error = result.error.expect("failed result includes diagnostics");
            assert!(error.contains("***"));
            assert!(!error.contains("secret-value"));
        } else {
            panic!("terminal result event expected");
        }

        let mut parser = AntigravityEventParser::default();
        parser.push_stderr(&"x".repeat(STDERR_DIAGNOSTIC_LIMIT + 100));
        parser.finish_stderr();
        let result = terminal(&mut parser, 1);
        if let ExecutionEventKind::Result { result } = result.kind {
            let error = result.error.expect("failed result includes diagnostics");
            let stderr = error
                .split_once("Antigravity stderr:\n")
                .expect("stderr marker is present")
                .1;
            assert_eq!(stderr.chars().count(), STDERR_DIAGNOSTIC_LIMIT);
        } else {
            panic!("terminal result event expected");
        }
    }
}
