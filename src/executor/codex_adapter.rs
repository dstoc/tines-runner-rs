//! Codex executor adapter and Codex-to-protocol event translation.

use serde_json::Value;
use std::collections::BTreeSet;

use crate::effort::EffortCapabilities;
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRateLimit, ExecutionRequest, ExecutionUsage,
    TerminalResult, TerminalStatus,
};
use crate::executor::codex::CodexLaunch;
use crate::executor::codex_stream::{CodexEvent, CodexRateLimit, CodexStreamParser};
use crate::executor::harness::{
    HarnessAdapter, HarnessAdapterError, HarnessEventParser, HarnessExit, HarnessLaunch,
};
use crate::executor::workspace::MaterializedWorkspace;

/// Initial executor-side adapter for the Codex CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodexAdapter;

impl HarnessAdapter for CodexAdapter {
    fn identifier(&self) -> &'static str {
        "codex"
    }

    fn launch(
        &self,
        request: &ExecutionRequest,
        workspace: &MaterializedWorkspace,
        capabilities: &EffortCapabilities,
    ) -> Result<HarnessLaunch, HarnessAdapterError> {
        if request.execution.harness != self.identifier() {
            return Err(HarnessAdapterError::new(
                "execution request does not match the Codex adapter",
            ));
        }
        let launch = CodexLaunch::for_execution_request(request, workspace, capabilities)
            .map_err(|_| HarnessAdapterError::new("could not prepare Codex launch"))?;
        let diagnostics = launch.format_diagnostics();
        Ok(HarnessLaunch::new(launch.command(), diagnostics))
    }

    fn event_parser(&self) -> Box<dyn HarnessEventParser> {
        Box::<CodexEventParser>::default()
    }
}

#[derive(Default)]
struct CodexEventParser {
    parser: CodexStreamParser,
    thread_ids: BTreeSet<String>,
    usage: Option<ExecutionUsage>,
    rate_limit: Option<CodexRateLimit>,
}

impl HarnessEventParser for CodexEventParser {
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

    fn terminal_result(&mut self, exit: HarnessExit) -> ExecutionEvent {
        let interrupted = exit.interrupted;
        let rate_limit = (!interrupted).then(|| self.rate_limit.clone()).flatten();
        let exit_code = exit
            .exit_code
            .filter(|code| *code != 0 || !interrupted)
            .filter(|code| rate_limit.is_none() || *code != 0);
        let (status, error, rate_limit_event) = if let Some(limit) = rate_limit {
            let error = limit
                .message
                .clone()
                .or(exit.error)
                .or_else(|| Some("provider usage limit reached".to_owned()));
            (
                TerminalStatus::RateLimited,
                error,
                Some(ExecutionRateLimit {
                    resume_at: limit.resume_at,
                    message: limit.message,
                }),
            )
        } else if !interrupted && exit_code == Some(0) {
            (TerminalStatus::Completed, None, None)
        } else {
            let error = exit.error.or_else(|| {
                if interrupted {
                    Some("harness execution was interrupted".to_owned())
                } else if let Some(code) = exit_code {
                    Some(format!("harness exited with code {code}"))
                } else {
                    Some("harness exited without a status".to_owned())
                }
            });
            (TerminalStatus::Failed, error, None)
        };

        ExecutionEvent::new(ExecutionEventKind::Result {
            result: TerminalResult {
                status,
                exit_code,
                error,
                provider_session_id: (self.thread_ids.len() == 1)
                    .then(|| self.thread_ids.iter().next().cloned())
                    .flatten(),
                usage: self.usage.clone(),
                pricing_evidence: None,
                interrupted,
                rate_limit: rate_limit_event,
            },
        })
    }
}

impl CodexEventParser {
    fn translate(&mut self, event: CodexEvent) -> Vec<ExecutionEvent> {
        let mut events = Vec::new();

        if let Some(limit) = event.rate_limit() {
            self.rate_limit = Some(limit.clone());
        }

        if let Some(id) = event.thread_id().filter(|id| !id.trim().is_empty()) {
            self.thread_ids.insert(id.to_owned());
            events.push(ExecutionEvent::new(ExecutionEventKind::Session {
                provider: "codex".to_owned(),
                id: id.to_owned(),
            }));
        }

        if let Some(error) = provider_error(&event) {
            events.push(ExecutionEvent::new(ExecutionEventKind::ProviderError {
                provider: "codex".to_owned(),
                code: error.code,
                message: error.message,
            }));
        }

        if let Some(limit) = event.rate_limit() {
            events.push(ExecutionEvent::new(ExecutionEventKind::RateLimit {
                rate_limit: ExecutionRateLimit {
                    resume_at: limit.resume_at,
                    message: limit.message,
                },
            }));
        }

        if let Some(usage) = event.raw_usage().and_then(normalize_usage) {
            self.usage = Some(usage.clone());
            events.push(ExecutionEvent::new(ExecutionEventKind::Usage { usage }));
        }

        events.extend(event.render_lines().into_iter().map(|message| {
            ExecutionEvent::new(ExecutionEventKind::Log {
                stream: crate::execution_protocol::LogStream::Stdout,
                message,
            })
        }));
        events
    }
}

struct ProviderError {
    code: Option<String>,
    message: String,
}

fn provider_error(event: &CodexEvent) -> Option<ProviderError> {
    let raw = event.raw()?;
    let error = match event.event_type()? {
        "turn.failed" => raw.get("error")?,
        "error" => raw,
        _ => return None,
    };
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    let code = ["codex_error_info", "code", "type"]
        .into_iter()
        .find_map(|key| error.get(key).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(|value| clip(value, 100));
    Some(ProviderError {
        code,
        message: clip(message, 2_000),
    })
}

fn normalize_usage(raw: &Value) -> Option<ExecutionUsage> {
    let usage = raw.as_object()?;
    let total_input = usage.get("input_tokens").and_then(Value::as_u64);
    let cache_read_tokens = usage.get("cached_input_tokens").and_then(Value::as_u64);
    let cache_write_tokens = usage
        .get("cache_write_input_tokens")
        .and_then(Value::as_u64);
    let input_tokens = total_input
        .zip(cache_read_tokens)
        .zip(cache_write_tokens)
        .and_then(|((total, read), write)| total.checked_sub(read)?.checked_sub(write));
    let normalized = ExecutionUsage {
        input_tokens,
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cache_read_tokens,
        cache_write_tokens,
    };
    (normalized.input_tokens.is_some()
        || normalized.output_tokens.is_some()
        || normalized.cache_read_tokens.is_some()
        || normalized.cache_write_tokens.is_some())
    .then_some(normalized)
}

fn clip(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}
