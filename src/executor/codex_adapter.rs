//! Codex executor adapter and Codex-to-protocol event translation.

use serde_json::Value;
use std::collections::BTreeSet;

use crate::effort::EffortCapabilities;
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionPricingEvidence, ExecutionRateLimit,
    ExecutionRequest, ExecutionUsage, TerminalResult, TerminalStatus,
};
use crate::executor::codex::CodexLaunch;
use crate::executor::codex_stream::{CodexEvent, CodexRateLimit, CodexStreamParser};
use crate::executor::harness::{
    HarnessAdapter, HarnessAdapterError, HarnessEventParser, HarnessExit, HarnessLaunch,
};
use crate::executor::workspace::MaterializedWorkspace;
use crate::protocol::{CodexMeasurementStatus, CodexPricingEvidenceV1, CodexRawUsageV1};

const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;

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

    fn event_parser_for_request(&self, request: &ExecutionRequest) -> Box<dyn HarnessEventParser> {
        Box::new(CodexEventParser::for_request(request))
    }
}

#[derive(Default)]
struct CodexEventParser {
    parser: CodexStreamParser,
    thread_ids: BTreeSet<String>,
    usage: Option<ExecutionUsage>,
    model: Option<String>,
    raw_usage: Option<CodexRawUsageV1>,
    measurement_status: CodexMeasurementStatus,
    sticky_status: Option<CodexMeasurementStatus>,
    terminal_snapshots: u64,
    model_rerouted: bool,
    rate_limit: Option<CodexRateLimit>,
    stderr_pending: String,
    stderr_secrets: Vec<String>,
    stderr_redaction_window: usize,
}

impl CodexEventParser {
    fn for_request(request: &ExecutionRequest) -> Self {
        let mut parser = Self::with_secrets(request.secret_patterns());
        parser.model = request.assignment.run.model.clone();
        parser
    }

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

    fn take_safe_stderr_prefix(&mut self, chunk: &str, finish: bool) -> String {
        self.stderr_pending.push_str(chunk);
        let characters = self.stderr_pending.chars().collect::<Vec<_>>();
        let mut safe_starts = if finish {
            characters.len()
        } else {
            characters
                .len()
                .saturating_sub(self.stderr_redaction_window)
        };
        if !finish
            && !self
                .stderr_secrets
                .iter()
                .any(|secret| secret.contains('\n'))
        {
            let complete_line = characters
                .iter()
                .rposition(|character| *character == '\n')
                .map_or(0, |index| index + 1);
            safe_starts = safe_starts.max(complete_line);
        }
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

    fn pricing_evidence(&self) -> CodexPricingEvidenceV1 {
        CodexPricingEvidenceV1 {
            version: 1,
            harness: "codex".to_owned(),
            model: self.model.clone(),
            identity_source: "launch_argument".to_owned(),
            usage_scope: "thread_total".to_owned(),
            session_mode: "cold".to_owned(),
            normalization: "codex-jsonl-v1".to_owned(),
            raw_usage: self.raw_usage.clone(),
            model_rerouted: self.model_rerouted,
            measurement_status: self.measurement_status,
            terminal_snapshots: self.terminal_snapshots,
            daemon_version: Some(crate::VERSION.to_owned()),
            request_context: None,
        }
    }

    fn record_usage(&mut self, raw: Option<&Value>) -> Option<ExecutionUsage> {
        let empty_usage = serde_json::Map::new();
        let object = raw.and_then(Value::as_object).unwrap_or(&empty_usage);
        let (raw_usage, status) = codex_pricing_usage(object);
        if status == CodexMeasurementStatus::Complete
            && is_nonmonotonic(&raw_usage, self.raw_usage.as_ref())
        {
            self.sticky_status = Some(CodexMeasurementStatus::Nonmonotonic);
        }
        self.raw_usage = Some(raw_usage);
        self.terminal_snapshots = self.terminal_snapshots.saturating_add(1);
        self.measurement_status = self.sticky_status.unwrap_or(status);
        raw.and_then(normalize_usage)
    }
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

    fn push_stderr(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix(chunk, false);
        log_stderr(&message)
    }

    fn finish_stderr(&mut self) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix("", true);
        log_stderr(&message)
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
                observed_model: None,
                usage: self.usage.clone(),
                pricing_evidence: Some(ExecutionPricingEvidence {
                    provider: "codex".to_owned(),
                    version: 1,
                    payload: serde_json::to_value(self.pricing_evidence())
                        .expect("Codex pricing evidence is serializable"),
                }),
                interrupted,
                rate_limit: rate_limit_event,
            },
        })
    }
}

fn log_stderr(message: &str) -> Vec<ExecutionEvent> {
    if message.is_empty() {
        return Vec::new();
    }
    vec![ExecutionEvent::new(ExecutionEventKind::Log {
        stream: crate::execution_protocol::LogStream::Stderr,
        message: message.to_owned(),
    })]
}

impl CodexEventParser {
    fn translate(&mut self, event: CodexEvent) -> Vec<ExecutionEvent> {
        let mut events = Vec::new();

        match event.event_type() {
            Some("turn.started") => {
                self.measurement_status = self
                    .sticky_status
                    .unwrap_or(CodexMeasurementStatus::IncompleteAttempt);
            }
            Some("item.completed")
                if event
                    .raw()
                    .and_then(|raw| raw.get("item"))
                    .is_some_and(is_model_reroute) =>
            {
                self.model_rerouted = true
            }
            _ => {}
        }

        if let Some(limit) = event.rate_limit() {
            self.rate_limit = Some(limit.clone());
        }

        if let Some(id) = event.thread_id().filter(|id| !id.trim().is_empty()) {
            self.thread_ids.insert(id.to_owned());
            if self.thread_ids.len() > 1 {
                self.sticky_status = Some(CodexMeasurementStatus::MultipleThreads);
                self.measurement_status = CodexMeasurementStatus::MultipleThreads;
            }
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

        if event.event_type() == Some("turn.completed") {
            let raw = event.raw().and_then(|event| event.get("usage"));
            if let Some(usage) = self.record_usage(raw) {
                self.usage = Some(usage.clone());
                events.push(ExecutionEvent::new(ExecutionEventKind::Usage { usage }));
            }
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

fn is_model_reroute(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("error")
        && item
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("model rerouted:"))
}

fn codex_pricing_usage(
    usage: &serde_json::Map<String, Value>,
) -> (CodexRawUsageV1, CodexMeasurementStatus) {
    let metric = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value <= MAX_SAFE_JSON_INTEGER)
    };
    let raw = CodexRawUsageV1 {
        input_tokens: metric("input_tokens"),
        cached_input_tokens: metric("cached_input_tokens"),
        cache_write_input_tokens: metric("cache_write_input_tokens"),
        output_tokens: metric("output_tokens"),
    };
    let supplied = [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
    ]
    .iter()
    .filter(|key| usage.contains_key(**key))
    .count();
    let complete = raw.input_tokens.is_some()
        && raw.cached_input_tokens.is_some()
        && raw.cache_write_input_tokens.is_some()
        && raw.output_tokens.is_some();
    let dimensions_reconcile = raw
        .cached_input_tokens
        .zip(raw.cache_write_input_tokens)
        .and_then(|(read, write)| read.checked_add(write))
        .zip(raw.input_tokens)
        .is_some_and(|(cached, total)| cached <= total);
    let status = if complete && dimensions_reconcile {
        CodexMeasurementStatus::Complete
    } else if supplied < 4 {
        CodexMeasurementStatus::Missing
    } else {
        CodexMeasurementStatus::Invalid
    };
    (raw, status)
}

fn is_nonmonotonic(current: &CodexRawUsageV1, previous: Option<&CodexRawUsageV1>) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let pairs = [
        (current.input_tokens, previous.input_tokens),
        (current.cached_input_tokens, previous.cached_input_tokens),
        (
            current.cache_write_input_tokens,
            previous.cache_write_input_tokens,
        ),
        (current.output_tokens, previous.output_tokens),
    ];
    if pairs
        .iter()
        .any(|(current, previous)| current.zip(*previous).is_some_and(|(a, b)| a < b))
    {
        return true;
    }
    let normalized_input = |usage: &CodexRawUsageV1| {
        usage
            .input_tokens?
            .checked_sub(usage.cached_input_tokens?)?
            .checked_sub(usage.cache_write_input_tokens?)
    };
    normalized_input(current)
        .zip(normalized_input(previous))
        .is_some_and(|(current, previous)| current < previous)
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
