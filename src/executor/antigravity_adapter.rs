//! Antigravity adapter and native stream-to-executor event translation.

use crate::effort::EffortCapabilities;
use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionRateLimit, ExecutionRequest, ExecutionUsage,
    LogStream, TerminalResult, TerminalStatus,
};
use crate::executor::antigravity::AntigravityLaunch;
use crate::executor::antigravity_stream::{AntigravityEvent, AntigravityStreamParser};
use crate::executor::harness::{
    HarnessAdapter, HarnessAdapterError, HarnessEventParser, HarnessExit, HarnessLaunch,
};
use crate::executor::workspace::MaterializedWorkspace;
use chrono::DateTime;
use serde_json::Value;

const STDERR_DIAGNOSTIC_LIMIT: usize = 8 * 1024;
const AGY_ERROR_RECORD_LIMIT: usize = STDERR_DIAGNOSTIC_LIMIT;
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
    response_redactor: StreamingSecretRedactor,
    stderr_pending: String,
    stderr_diagnostic: String,
    stderr_record_pending: String,
    stderr_record_oversized: bool,
    provider_error: Option<AntigravityProviderError>,
    rate_limit: Option<ExecutionRateLimit>,
    secret_patterns: Vec<String>,
    stderr_redaction_window: usize,
}

#[derive(Clone, Debug)]
struct AntigravityTerminal {
    status: String,
    error: Option<String>,
    denied_actions: bool,
}

#[derive(Clone, Debug)]
struct AntigravityProviderError {
    status: Option<String>,
    http_code: Option<String>,
    grpc_code: Option<String>,
    retryable: Option<bool>,
    error_id: Option<String>,
    message: Option<String>,
    resume_at: Option<u64>,
    rate_limited: bool,
}

impl AntigravityProviderError {
    fn code(&self, secrets: &[String]) -> Option<String> {
        self.status
            .clone()
            .or_else(|| self.http_code.as_ref().map(|code| format!("HTTP_{code}")))
            .or_else(|| self.grpc_code.as_ref().map(|code| format!("gRPC_{code}")))
            .or_else(|| self.error_id.clone())
            .map(|code| clip(&redact_complete(&code, secrets), 100))
    }

    fn is_retryable(&self) -> bool {
        self.rate_limited || self.retryable == Some(true)
    }

    fn diagnostic(&self, secrets: &[String]) -> String {
        let field = |value: &str, limit| clip(&redact_complete(value, secrets), limit);
        let mut details = Vec::new();
        if let Some(status) = &self.status {
            details.push(format!("status={}", field(status, 80)));
        }
        if let Some(code) = &self.http_code {
            details.push(format!("http_code={}", field(code, 16)));
        }
        if let Some(code) = &self.grpc_code {
            details.push(format!("grpc_code={}", field(code, 40)));
        }
        if let Some(retryable) = self.retryable {
            details.push(format!("retryable={retryable}"));
        }
        if let Some(id) = &self.error_id {
            details.push(format!("error_id={}", field(id, 120)));
        }
        if let Some(message) = &self.message {
            details.push(field(message, 1_600));
        }
        let details = if details.is_empty() {
            "provider failure".to_owned()
        } else {
            details.join(" ")
        };
        clip(&format!("AGY_ERROR {details}"), 2_000)
    }
}

impl AntigravityEventParser {
    fn with_secrets(stderr_secrets: Vec<String>) -> Self {
        let secret_patterns = ordered_patterns(stderr_secrets);
        let stderr_redaction_window = secret_patterns
            .iter()
            .map(|secret| secret.chars().count())
            .max()
            .unwrap_or_default()
            .saturating_sub(1);
        Self {
            response_redactor: StreamingSecretRedactor::new(secret_patterns.clone()),
            secret_patterns,
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
                            let safe_delta = self.response_redactor.push(delta);
                            events.extend(log_text(LogStream::Stdout, &safe_delta));
                        }
                    }
                    Some("tool") => {
                        let name = step
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .or_else(|| step.pointer("/tool_info/name").and_then(Value::as_str));
                        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
                            let name = redact_complete(name, &self.secret_patterns);
                            let state = step.get("state").and_then(Value::as_str);
                            let message = state.map_or_else(
                                || format!("[tool] {}", clip(&name, 160)),
                                |state| {
                                    let state = redact_complete(state, &self.secret_patterns);
                                    format!("[tool] {} ({})", clip(&name, 160), clip(&state, 32))
                                },
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
                    .map(|error| clip(&redact_complete(error, &self.secret_patterns), 2_000));
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
                    let response = redact_complete(response, &self.secret_patterns);
                    events.extend(log_text(LogStream::Stdout, &response));
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
                .secret_patterns
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

    fn observe_stderr_records(&mut self, text: &str, finish: bool) -> Vec<ExecutionEvent> {
        let mut events = Vec::new();
        for character in text.chars() {
            if self.stderr_record_oversized {
                if character == '\n' {
                    self.stderr_record_oversized = false;
                }
                continue;
            }
            if character == '\n' {
                let line = std::mem::take(&mut self.stderr_record_pending);
                events.extend(self.parse_stderr_record(&line));
                continue;
            }
            self.stderr_record_pending.push(character);
            if self.stderr_record_pending.len() > AGY_ERROR_RECORD_LIMIT {
                self.stderr_record_pending.clear();
                self.stderr_record_oversized = true;
            }
        }
        if finish && !self.stderr_record_oversized {
            let line = std::mem::take(&mut self.stderr_record_pending);
            events.extend(self.parse_stderr_record(&line));
        }
        events
    }

    fn parse_stderr_record(&mut self, line: &str) -> Vec<ExecutionEvent> {
        let Some(error) = parse_agy_error(line) else {
            return Vec::new();
        };
        let diagnostic = error.diagnostic(&self.secret_patterns);
        // Protocol v1 uses its rate-limit judgment for retryable provider failures.
        let rate_limit = error.is_retryable().then(|| ExecutionRateLimit {
            resume_at: error.resume_at,
            message: Some(diagnostic.clone()),
        });
        let event = ExecutionEvent::new(ExecutionEventKind::ProviderError {
            provider: "antigravity".to_owned(),
            code: error.code(&self.secret_patterns),
            message: diagnostic,
        });
        self.provider_error = Some(error);
        self.rate_limit = rate_limit.clone();
        let mut events = vec![event];
        if let Some(rate_limit) = rate_limit {
            events.push(ExecutionEvent::new(ExecutionEventKind::RateLimit {
                rate_limit,
            }));
        }
        events
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
        let mut events = self
            .parser
            .finish()
            .into_iter()
            .flat_map(|event| self.translate(event))
            .collect::<Vec<_>>();
        let response_tail = self.response_redactor.finish();
        events.extend(log_text(LogStream::Stdout, &response_tail));
        events
    }

    fn push_stderr(&mut self, chunk: &str) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix(chunk, false);
        self.retain_stderr(&message);
        let mut events = log_stderr(&message);
        events.extend(self.observe_stderr_records(&message, false));
        events
    }

    fn finish_stderr(&mut self) -> Vec<ExecutionEvent> {
        let message = self.take_safe_stderr_prefix("", true);
        self.retain_stderr(&message);
        let mut events = log_stderr(&message);
        events.extend(self.observe_stderr_records(&message, true));
        events
    }

    fn terminal_result(&mut self, exit: HarnessExit) -> ExecutionEvent {
        let stderr_tail = self.take_safe_stderr_prefix("", true);
        self.retain_stderr(&stderr_tail);
        drop(self.observe_stderr_records(&stderr_tail, true));
        let terminal = self.terminal.clone();
        let provider_interrupted = terminal
            .as_ref()
            .is_some_and(|result| matches!(result.status.as_str(), "CANCELED" | "INTERRUPTED"));
        let interrupted = exit.interrupted || provider_interrupted;
        let success = !interrupted
            && !self.malformed
            && self.provider_error.is_none()
            && exit.exit_code == Some(0)
            && exit.error.is_none()
            && terminal
                .as_ref()
                .is_some_and(|result| result.status == "SUCCESS" && !result.denied_actions);
        let rate_limit = (!interrupted).then(|| self.rate_limit.clone()).flatten();
        let status = if rate_limit.is_some() {
            TerminalStatus::RateLimited
        } else if success {
            TerminalStatus::Completed
        } else {
            TerminalStatus::Failed
        };
        let error = if success {
            None
        } else if let Some(provider_error) = &self.provider_error {
            let mut message = provider_error.diagnostic(&self.secret_patterns);
            if !self.stderr_diagnostic.is_empty() {
                message.push_str("\nAntigravity stderr:\n");
                message.push_str(&self.stderr_diagnostic);
            }
            Some(message)
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
                        terminal.as_ref().map(|result| {
                            format!(
                                "Antigravity returned status {}",
                                redact_complete(&result.status, &self.secret_patterns)
                            )
                        })
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
                exit_code: if success {
                    Some(0)
                } else if interrupted {
                    None
                } else {
                    exit.exit_code.filter(|code| *code != 0)
                },
                error,
                provider_session_id: self.conversation_id.clone(),
                usage: self.usage.clone(),
                pricing_evidence: None,
                interrupted,
                rate_limit,
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

fn parse_agy_error(line: &str) -> Option<AntigravityProviderError> {
    let payload = line.trim().strip_prefix("AGY_ERROR:")?.trim();
    let value: Value = serde_json::from_str(payload).ok()?;
    let object = value.as_object()?;
    let status = string_field(
        object,
        &[
            "canonical_status",
            "canonicalStatus",
            "canonical_code",
            "canonicalCode",
            "provider_status",
            "providerStatus",
            "status",
        ],
    );
    let http_code = scalar_field(
        object,
        &[
            "http_code",
            "http_status",
            "http_status_code",
            "httpCode",
            "httpStatus",
            "httpStatusCode",
        ],
    );
    let grpc_code = scalar_field(
        object,
        &[
            "grpc_code",
            "grpc_status",
            "grpc_status_code",
            "grpcCode",
            "grpcStatus",
            "grpcStatusCode",
        ],
    );
    let retryable = object.get("retryable").and_then(Value::as_bool);
    let error_id = string_field(
        object,
        &[
            "error_id",
            "errorId",
            "provider_error_id",
            "providerErrorId",
            "model_error_id",
            "modelErrorId",
        ],
    );
    let message = string_field(object, &["message", "short_error", "shortError", "error"]);
    let resume_at = [
        "resume_at",
        "resumeAt",
        "retry_at",
        "retryAt",
        "reset_at",
        "resetAt",
        "resets_at",
        "resetsAt",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(parse_provider_timestamp));
    let rate_limited = is_rate_limit_error(
        status.as_deref(),
        http_code.as_deref(),
        grpc_code.as_deref(),
    );

    (status.is_some()
        || http_code.is_some()
        || grpc_code.is_some()
        || retryable.is_some()
        || error_id.is_some()
        || message.is_some())
    .then_some(AntigravityProviderError {
        status,
        http_code,
        grpc_code,
        retryable,
        error_id,
        message,
        resume_at,
        rate_limited,
    })
}

fn string_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn scalar_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match object.get(*key)? {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn is_rate_limit_error(
    status: Option<&str>,
    http_code: Option<&str>,
    grpc_code: Option<&str>,
) -> bool {
    let normalized = |value: &str| value.trim().to_ascii_uppercase().replace(['-', ' '], "_");
    http_code.is_some_and(|code| code.trim() == "429")
        || status.is_some_and(|status| {
            matches!(
                normalized(status).as_str(),
                "RESOURCE_EXHAUSTED"
                    | "QUOTA_EXCEEDED"
                    | "RATE_LIMIT_EXCEEDED"
                    | "TOO_MANY_REQUESTS"
            )
        })
        || grpc_code
            .is_some_and(|code| matches!(normalized(code).as_str(), "8" | "RESOURCE_EXHAUSTED"))
}

fn parse_provider_timestamp(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return normalize_provider_timestamp(number);
    }
    let value = value.as_str()?.trim();
    value
        .parse::<u64>()
        .ok()
        .and_then(normalize_provider_timestamp)
        .or_else(|| {
            u64::try_from(DateTime::parse_from_rfc3339(value).ok()?.timestamp_millis())
                .ok()
                .filter(|timestamp| *timestamp <= MAX_SAFE_JSON_INTEGER)
        })
}

fn normalize_provider_timestamp(value: u64) -> Option<u64> {
    if value == 0 {
        None
    } else if value < 1_000_000_000_000 {
        value
            .checked_mul(1_000)
            .filter(|timestamp| *timestamp <= MAX_SAFE_JSON_INTEGER)
    } else {
        (value <= MAX_SAFE_JSON_INTEGER).then_some(value)
    }
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

fn ordered_patterns(mut patterns: Vec<String>) -> Vec<String> {
    patterns.retain(|pattern| !pattern.is_empty());
    patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
    patterns.dedup();
    patterns
}

/// A streaming redactor that keeps any possible secret suffix until the next
/// chunk can prove that it is safe to emit.
#[derive(Default)]
struct StreamingSecretRedactor {
    patterns: Vec<String>,
    pending: String,
}

impl StreamingSecretRedactor {
    fn new(patterns: Vec<String>) -> Self {
        let patterns = ordered_patterns(patterns);
        Self {
            patterns,
            pending: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> String {
        let characters = self.pending.chars().collect::<Vec<_>>();
        let mut safe_chars = characters.len();

        if !finish {
            for pattern in &self.patterns {
                let prefix_bytes = pattern.char_indices().map(|(index, _)| index).skip(1);
                for prefix_end in prefix_bytes {
                    if self.pending.ends_with(&pattern[..prefix_end]) {
                        safe_chars = safe_chars
                            .min(characters.len() - pattern[..prefix_end].chars().count());
                    }
                }
            }
        }

        loop {
            let previous_safe_chars = safe_chars;
            for (start, (byte_start, _)) in self.pending.char_indices().enumerate() {
                if start >= safe_chars {
                    break;
                }
                for pattern in &self.patterns {
                    if self.pending[byte_start..].starts_with(pattern) {
                        let end = start + pattern.chars().count();
                        if end > safe_chars {
                            safe_chars = start;
                        }
                    }
                }
            }
            if safe_chars == previous_safe_chars {
                break;
            }
        }

        let safe_bytes = characters
            .iter()
            .take(safe_chars)
            .map(|character| character.len_utf8())
            .sum::<usize>();
        let safe = self.pending[..safe_bytes].to_owned();
        self.pending.drain(..safe_bytes);
        redact_complete(&safe, &self.patterns)
    }
}

/// Replace every matching range, including ranges that overlap another
/// secret. This prevents either secret from leaving a visible suffix.
fn redact_complete(text: &str, patterns: &[String]) -> String {
    let mut ranges = Vec::new();
    for (start, _) in text.char_indices() {
        let suffix = &text[start..];
        for pattern in patterns {
            if suffix.starts_with(pattern) {
                ranges.push((start, start + pattern.len()));
            }
        }
    }
    if ranges.is_empty() {
        return text.to_owned();
    }
    ranges.sort_unstable();

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut range = ranges[0];
    for next in ranges.into_iter().skip(1) {
        if next.0 <= range.1 {
            range.1 = range.1.max(next.1);
            continue;
        }
        redacted.push_str(&text[cursor..range.0]);
        redacted.push_str("***");
        cursor = range.1;
        range = next;
    }
    redacted.push_str(&text[cursor..range.0]);
    redacted.push_str("***");
    redacted.push_str(&text[range.1..]);
    redacted
}

#[cfg(test)]
mod tests {
    use super::{
        AGY_ERROR_RECORD_LIMIT, AntigravityEventParser, HarnessEventParser, HarnessExit,
        STDERR_DIAGNOSTIC_LIMIT,
    };
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

    fn stderr(
        parser: &mut AntigravityEventParser,
        text: &str,
    ) -> Vec<crate::execution_protocol::ExecutionEvent> {
        let mut events = parser.push_stderr(text);
        events.extend(parser.finish_stderr());
        events
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
    fn redacts_split_and_overlapping_response_secrets_before_logging() {
        let mut parser = AntigravityEventParser::with_secrets(vec![
            "secret-value".to_owned(),
            "synthetic-secret-value".to_owned(),
        ]);
        let first = parser.push(
            "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"before synthetic-secret-\"}}\n",
        );
        let second = parser.push(
            "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"value after\"}}\n",
        );
        let mut events = first;
        events.extend(second);
        events.extend(parser.finish());
        let text = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Log {
                    stream: LogStream::Stdout,
                    message,
                } => Some(message.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "before *** after");
        assert!(!text.contains("synthetic-secret-value"));
        assert!(!text.contains("secret-value"));
    }

    #[test]
    fn redacts_terminal_fallback_before_splitting_log_chunks() {
        const CHUNK_CHARS: usize = 32 * 1024;
        let secret = "synthetic-secret-value";
        let prefix = "x".repeat(CHUNK_CHARS - 8);
        let response = format!("{prefix}{secret}tail");
        let raw = serde_json::json!({
            "event": "result",
            "result": { "status": "SUCCESS", "response": response }
        });
        let stream = format!("{}\n", raw);
        let mut parser = AntigravityEventParser::with_secrets(vec![secret.to_owned()]);
        let mut events = parser.push(&stream);
        events.extend(parser.finish());
        let logs = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Log {
                    stream: LogStream::Stdout,
                    message,
                } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            logs.iter().map(|log| log.chars().count()).sum::<usize>(),
            prefix.len() + 3 + 4
        );
        assert!(logs.iter().all(|log| !log.contains(secret)));
        assert!(logs.iter().all(|log| !log.contains("syntheti")));
        assert!(logs.iter().any(|log| log.contains("***")));
    }

    #[test]
    fn redacts_provider_and_clipped_tool_fields_before_truncation() {
        let secret = "SYNTHETIC_REVIEW_SECRET_VALUE";
        let tool_secret = "TOOL_REDACTION_SECRET_VALUE";
        let raw = serde_json::json!({
            "event": "step_update",
            "step_update": {
                "step_type": "tool",
                "tool_name": format!("{}{}", "x".repeat(152), tool_secret),
                "state": format!("{}{}", "y".repeat(24), tool_secret)
            }
        });
        let result = serde_json::json!({
            "event": "result",
            "result": {
                "status": "ERROR",
                "error": format!("{}{}", "f".repeat(1_990), secret)
            }
        });
        let stream = format!("{}\n{}\n", raw, result);
        let mut parser =
            AntigravityEventParser::with_secrets(vec![secret.to_owned(), tool_secret.to_owned()]);
        let mut events = parser.push(&stream);
        events.extend(parser.finish());
        let rendered = serde_json::to_string(&events).expect("events serialize");
        assert!(rendered.contains("***"));
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("SYNTHETIC"));
        assert!(!rendered.contains("TOOL_RED"));
        let terminal = terminal(&mut parser, 0);
        let ExecutionEventKind::Result { result } = terminal.kind else {
            panic!("terminal result event expected");
        };
        let error = result.error.expect("provider failure includes error");
        assert!(!error.contains(secret));
        assert!(!error.contains("SYNTHETIC"));
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
            result.validate().expect("failed status is a valid result");
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

    #[test]
    fn classifies_structured_http_429_and_quota_errors_as_rate_limited() {
        let mut parser = AntigravityEventParser::default();
        let events = stderr(
            &mut parser,
            "AGY_ERROR: {\"canonical_status\":\"RESOURCE_EXHAUSTED\",\"http_code\":429,\"grpc_code\":8,\"retryable\":false,\"error_id\":\"provider-17\",\"retry_at\":2000000000,\"message\":\"provider quota reached\"}\n",
        );
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ExecutionEventKind::ProviderError { code: Some(code), message, .. }
                if code == "RESOURCE_EXHAUSTED"
                    && message.contains("http_code=429")
                    && message.contains("grpc_code=8")
                    && message.contains("error_id=provider-17")
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ExecutionEventKind::RateLimit { rate_limit }
                if rate_limit.resume_at == Some(2_000_000_000_000)
                    && rate_limit.message.as_deref().is_some_and(|message| message.contains("provider quota reached"))
        )));
        assert!(matches!(
            terminal(&mut parser, 3).kind,
            ExecutionEventKind::Result { result }
                if result.status == TerminalStatus::RateLimited
                    && result.rate_limit.as_ref().is_some_and(|limit| limit.resume_at == Some(2_000_000_000_000))
        ));

        let mut parser = AntigravityEventParser::default();
        let events = stderr(
            &mut parser,
            "AGY_ERROR: {\"canonical_status\":\"QUOTA_EXCEEDED\",\"retryable\":false,\"message\":\"daily quota reached\"}\n",
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, ExecutionEventKind::RateLimit { .. }))
        );
        assert!(matches!(
            terminal(&mut parser, 3).kind,
            ExecutionEventKind::Result { result } if result.status == TerminalStatus::RateLimited
        ));
    }

    #[test]
    fn classifies_retryable_transient_errors_but_keeps_deterministic_errors_failed() {
        let mut parser = AntigravityEventParser::default();
        let events = stderr(
            &mut parser,
            "AGY_ERROR: {\"canonical_status\":\"UNAVAILABLE\",\"http_code\":503,\"retryable\":true,\"error_id\":\"transient-503\"}\n",
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, ExecutionEventKind::RateLimit { .. }))
        );
        assert!(matches!(
            terminal(&mut parser, 3).kind,
            ExecutionEventKind::Result { result }
                if result.status == TerminalStatus::RateLimited
                    && result.error.as_deref().is_some_and(|error| error.contains("retryable=true"))
        ));

        let mut parser = AntigravityEventParser::default();
        let events = stderr(
            &mut parser,
            "AGY_ERROR: {\"canonical_status\":\"NOT_FOUND\",\"http_code\":404,\"retryable\":false,\"error_id\":\"model-not-found\"}\n",
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, ExecutionEventKind::RateLimit { .. }))
        );
        assert!(matches!(
            terminal(&mut parser, 3).kind,
            ExecutionEventKind::Result { result }
                if result.status == TerminalStatus::Failed
                    && result.rate_limit.is_none()
                    && result.error.as_deref().is_some_and(|error| error.contains("NOT_FOUND"))
        ));
    }

    #[test]
    fn malformed_unknown_and_oversized_provider_records_fall_back_to_stderr() {
        let mut parser = AntigravityEventParser::default();
        let events = stderr(
            &mut parser,
            "ordinary diagnostic\nAGY_ERROR: not-json\nAGY_ERROR: {\"future_field\":\"value\"}\n",
        );
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            ExecutionEventKind::ProviderError { .. } | ExecutionEventKind::RateLimit { .. }
        )));
        let logs = events
            .iter()
            .filter_map(|event| match &event.kind {
                ExecutionEventKind::Log {
                    stream: LogStream::Stderr,
                    message,
                } => Some(message.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(logs.contains("ordinary diagnostic"));
        assert!(logs.contains("AGY_ERROR: not-json"));

        let mut parser = AntigravityEventParser::default();
        let oversized = format!(
            "AGY_ERROR: {{\"retryable\":true,\"message\":\"{}\"}}\n",
            "x".repeat(AGY_ERROR_RECORD_LIMIT)
        );
        let events = stderr(&mut parser, &oversized);
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            ExecutionEventKind::ProviderError { .. } | ExecutionEventKind::RateLimit { .. }
        )));
        assert!(parser.stderr_diagnostic.chars().count() <= STDERR_DIAGNOSTIC_LIMIT);
    }

    #[test]
    fn redacts_structured_fields_before_events_and_terminal_diagnostics() {
        let secret = "synthetic-provider-secret";
        let raw = format!(
            "AGY_ERROR: {{\"canonical_status\":\"{secret}\",\"retryable\":true,\"error_id\":\"{secret}\",\"message\":\"quota {secret}\"}}\n"
        );
        let split = raw.find(secret).expect("secret in record") + 9;
        let mut parser = AntigravityEventParser::with_secrets(vec![secret.to_owned()]);
        let mut events = parser.push_stderr(&raw[..split]);
        events.extend(parser.push_stderr(&raw[split..]));
        events.extend(parser.finish_stderr());
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ExecutionEventKind::ProviderError { code: Some(code), .. } if code == "***"
        )));
        let result = terminal(&mut parser, 3);
        let rendered = format!(
            "{}{}",
            serde_json::to_string(&events).expect("serialize structured events"),
            serde_json::to_string(&result).expect("serialize terminal result")
        );
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("***"));
        let ExecutionEventKind::Result { result } = result.kind else {
            panic!("terminal result event expected");
        };
        let error = result.error.expect("provider failure includes diagnostic");
        assert!(error.contains("***"));
        assert!(error.chars().count() <= STDERR_DIAGNOSTIC_LIMIT + 2_100);
        assert_eq!(result.status, TerminalStatus::RateLimited);
    }
}
