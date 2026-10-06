//! Daemon-side translation of generic executor events into Tines run data.

use serde_json::{Map, Value};
use std::fmt;

use crate::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionEventParser, ExecutionPricingEvidence,
    ExecutionRateLimit, ExecutionUsage, LogStream, ProtocolError, TerminalResult, TerminalStatus,
};
use crate::protocol::{
    CodexPricingEvidenceV1, FinishJudgment, FinishRunRequest, FinishStatus, RunUsage,
};

/// Incremental executor protocol state and the latest reportable run metadata.
pub struct ExecutorEventStream {
    parser: ExecutionEventParser,
    report: ExecutorRunReport,
}

impl ExecutorEventStream {
    /// Create a stream that removes assignment secrets from every event field.
    pub fn new(request: &crate::execution_protocol::ExecutionRequest) -> Self {
        Self::new_with_secrets(request, &[])
    }

    /// Create a stream with daemon-side secrets that are not present in the request.
    pub fn new_with_secrets(
        request: &crate::execution_protocol::ExecutionRequest,
        additional_secrets: &[&str],
    ) -> Self {
        Self {
            parser: ExecutionEventParser::default(),
            report: ExecutorRunReport::new(request, additional_secrets),
        }
    }

    /// Parse one arbitrary stdout chunk and return normalized log text.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, ExecutorEventPushError> {
        let mut logs = Vec::new();
        let mut start = 0;
        while let Some(offset) = chunk[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset + 1;
            self.push_record(&chunk[start..end], &mut logs)?;
            start = end;
        }
        if start < chunk.len() {
            self.push_record(&chunk[start..], &mut logs)?;
        }
        Ok(logs)
    }

    fn push_record(
        &mut self,
        record: &[u8],
        logs: &mut Vec<String>,
    ) -> Result<(), ExecutorEventPushError> {
        let events = self
            .parser
            .push(record)
            .map_err(|error| ExecutorEventPushError {
                logs: std::mem::take(logs),
                error,
            })?;
        logs.extend(
            events
                .into_iter()
                .filter_map(|event| self.report.observe(event)),
        );
        Ok(())
    }

    /// Require a terminal result at EOF and include a final unterminated line.
    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        if let Some(event) = self.parser.finish()? {
            self.report.observe(event);
        }
        Ok(())
    }

    /// Build a Tines finish request from events seen before EOF or failure.
    pub fn finish_request(
        &self,
        failure: Option<&str>,
        stderr: &str,
        interrupted: bool,
    ) -> FinishRunRequest {
        self.report.finish_request(failure, stderr, interrupted)
    }
}

/// A protocol failure and any logs parsed earlier in the same stdout chunk.
#[derive(Debug)]
pub struct ExecutorEventPushError {
    pub logs: Vec<String>,
    error: ProtocolError,
}

impl fmt::Display for ExecutorEventPushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for ExecutorEventPushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

struct ExecutorRunReport {
    secrets: Vec<String>,
    latest_session: Option<String>,
    latest_usage: Option<ExecutionUsage>,
    latest_pricing: Option<ExecutionPricingEvidence>,
    latest_rate_limit: Option<ExecutionRateLimit>,
    latest_provider_error: Option<String>,
    result: Option<TerminalResult>,
}

impl ExecutorRunReport {
    fn new(
        request: &crate::execution_protocol::ExecutionRequest,
        additional_secrets: &[&str],
    ) -> Self {
        let mut secrets = request
            .assignment
            .run_key
            .iter()
            .map(String::as_str)
            .chain(additional_secrets.iter().copied())
            .chain(
                request
                    .assignment
                    .env
                    .iter()
                    .filter(|entry| entry.secret)
                    .map(|entry| entry.value.as_str()),
            )
            .filter(|secret| !secret.is_empty())
            .flat_map(|secret| {
                let json_escaped =
                    serde_json::to_string(secret).expect("Rust strings serialize to JSON");
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

        Self {
            secrets,
            latest_session: None,
            latest_usage: None,
            latest_pricing: None,
            latest_rate_limit: None,
            latest_provider_error: None,
            result: None,
        }
    }

    fn observe(&mut self, mut event: ExecutionEvent) -> Option<String> {
        self.redact_event(&mut event);
        match event.kind {
            ExecutionEventKind::Log { stream, message } => render_log(stream, &message),
            ExecutionEventKind::Session { id, .. } => {
                self.latest_session = Some(id);
                None
            }
            ExecutionEventKind::ProviderError { message, .. } => {
                self.latest_provider_error = Some(message);
                None
            }
            ExecutionEventKind::Usage { usage } => {
                self.latest_usage = Some(usage);
                None
            }
            ExecutionEventKind::RateLimit { rate_limit } => {
                self.latest_rate_limit = Some(rate_limit);
                None
            }
            ExecutionEventKind::Result { result } => {
                if result.provider_session_id.is_some() {
                    self.latest_session = result.provider_session_id.clone();
                }
                if result.usage.is_some() {
                    self.latest_usage = result.usage.clone();
                }
                if result.pricing_evidence.is_some() {
                    self.latest_pricing = result.pricing_evidence.clone();
                }
                if result.rate_limit.is_some() {
                    self.latest_rate_limit = result.rate_limit.clone();
                }
                self.result = Some(result);
                None
            }
        }
    }

    fn redact_event(&self, event: &mut ExecutionEvent) {
        match &mut event.kind {
            ExecutionEventKind::Log { message, .. } => self.redact_string(message),
            ExecutionEventKind::Session { provider, id } => {
                self.redact_string(provider);
                self.redact_string(id);
            }
            ExecutionEventKind::ProviderError {
                provider,
                code,
                message,
            } => {
                self.redact_string(provider);
                if let Some(code) = code {
                    self.redact_string(code);
                }
                self.redact_string(message);
            }
            ExecutionEventKind::Usage { .. } => {}
            ExecutionEventKind::RateLimit { rate_limit } => {
                if let Some(message) = &mut rate_limit.message {
                    self.redact_string(message);
                }
            }
            ExecutionEventKind::Result { result } => {
                if let Some(error) = &mut result.error {
                    self.redact_string(error);
                }
                if let Some(session) = &mut result.provider_session_id {
                    self.redact_string(session);
                }
                if let Some(rate_limit) = &mut result.rate_limit
                    && let Some(message) = &mut rate_limit.message
                {
                    self.redact_string(message);
                }
                if let Some(pricing) = &mut result.pricing_evidence {
                    self.redact_string(&mut pricing.provider);
                    self.redact_value(&mut pricing.payload);
                }
            }
        }
    }

    fn redact_string(&self, value: &mut String) {
        for secret in &self.secrets {
            *value = value.replace(secret, "[REDACTED]");
        }
    }

    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => self.redact_string(text),
            Value::Array(values) => values.iter_mut().for_each(|value| self.redact_value(value)),
            Value::Object(values) => {
                let mut redacted = Map::new();
                for (mut key, mut value) in std::mem::take(values) {
                    self.redact_string(&mut key);
                    self.redact_value(&mut value);
                    redacted.insert(key, value);
                }
                *values = redacted;
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn finish_request(
        &self,
        failure: Option<&str>,
        stderr: &str,
        interrupted: bool,
    ) -> FinishRunRequest {
        let result = self.result.as_ref();
        let interrupted = interrupted || result.is_some_and(|result| result.interrupted);
        let completed = failure.is_none()
            && !interrupted
            && result.is_some_and(|result| result.status == TerminalStatus::Completed);
        let status = if completed {
            FinishStatus::Completed
        } else {
            FinishStatus::Failed
        };

        let rate_limit = result
            .and_then(|result| result.rate_limit.as_ref())
            .or(self.latest_rate_limit.as_ref());
        let judgment = if interrupted {
            Some(FinishJudgment::Interrupted)
        } else if status == FinishStatus::Failed
            && (result.is_some_and(|result| result.status == TerminalStatus::RateLimited)
                || rate_limit.is_some())
        {
            Some(FinishJudgment::RateLimited)
        } else {
            None
        };

        let mut error = if completed {
            None
        } else {
            failure
                .map(str::to_owned)
                .or_else(|| result.and_then(|result| result.error.clone()))
                .or_else(|| self.latest_provider_error.clone())
                .or_else(|| {
                    result.and_then(|result| {
                        result.exit_code.map(|code| {
                            format!("executor reported a failed result with exit code {code}")
                        })
                    })
                })
                .or_else(|| {
                    rate_limit
                        .and_then(|rate_limit| rate_limit.message.clone())
                        .map(|message| format!("provider rate limit: {message}"))
                })
                .or_else(|| Some("executor reported a failed result".to_owned()))
        };
        if let Some(error) = &mut error {
            self.redact_string(error);
        }
        let mut stderr = stderr.trim().to_owned();
        self.redact_string(&mut stderr);
        if !completed && !stderr.is_empty() {
            let diagnostic = format!("Executor stderr:\n{stderr}");
            error = Some(match error {
                Some(error) if !error.is_empty() => format!("{error}\n{diagnostic}"),
                _ => diagnostic,
            });
        }

        let resume_at = (judgment == Some(FinishJudgment::RateLimited))
            .then(|| rate_limit.and_then(|rate_limit| rate_limit.resume_at))
            .flatten();
        FinishRunRequest {
            status,
            error,
            provider_session_id: result
                .and_then(|result| result.provider_session_id.clone())
                .or_else(|| self.latest_session.clone()),
            usage: result
                .and_then(|result| result.usage.as_ref())
                .or(self.latest_usage.as_ref())
                .map(to_run_usage),
            pricing_evidence: result
                .and_then(|result| result.pricing_evidence.as_ref())
                .or(self.latest_pricing.as_ref())
                .and_then(to_codex_pricing_evidence),
            judgment,
            resume_at,
        }
    }
}

fn render_log(stream: LogStream, message: &str) -> Option<String> {
    if message.is_empty() {
        return None;
    }
    let rendered = match stream {
        LogStream::Stdout => message.to_owned(),
        LogStream::Stderr => format!("[stderr] {message}"),
        LogStream::System => format!("[system] {message}"),
    };
    if rendered.ends_with('\n') {
        Some(rendered)
    } else {
        Some(format!("{rendered}\n"))
    }
}

fn to_run_usage(usage: &ExecutionUsage) -> RunUsage {
    RunUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
    }
}

fn to_codex_pricing_evidence(
    evidence: &ExecutionPricingEvidence,
) -> Option<CodexPricingEvidenceV1> {
    if evidence.provider != "codex" || evidence.version != 1 {
        return None;
    }
    serde_json::from_value(evidence.payload.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::ExecutorEventStream;
    use crate::execution_protocol::ExecutionRequest;
    use crate::protocol::{FinishJudgment, FinishStatus};
    use serde_json::{Value, json};

    #[test]
    fn review_metadata_and_logs_before_protocol_failure_survive_chunk_boundaries() {
        let mut prefix =
            line(json!({"version":1,"type":"log","stream":"stdout","message":"before error"}));
        prefix.extend(line(
            json!({"version":1,"type":"session","provider":"codex","id":"thread-before-error"}),
        ));
        prefix.extend(line(
            json!({"version":1,"type":"usage","input_tokens":17,"output_tokens":9}),
        ));
        prefix.extend(line(json!({"version":1,"type":"rate_limit","resume_at":2_000_000_000_000_u64,"message":"try later"})));
        let mut together = ExecutorEventStream::new(&request());
        let mut split = ExecutorEventStream::new(&request());
        let mut combined = prefix.clone();
        combined.extend_from_slice(b"not-json\n");
        let together_error = together.push(&combined).expect_err("malformed suffix");
        let split_logs = split.push(&prefix).expect("valid event prefix");
        let split_error = split.push(b"not-json\n").expect_err("malformed suffix");
        assert_eq!(together_error.logs, split_logs);
        assert!(split_error.logs.is_empty());
        assert_eq!(together_error.to_string(), split_error.to_string());
        let together_finish = together.finish_request(Some("protocol failure"), "", false);
        let split_finish = split.finish_request(Some("protocol failure"), "", false);
        assert_eq!(
            together_finish.provider_session_id,
            split_finish.provider_session_id
        );
        assert_eq!(together_finish.usage, split_finish.usage);
        assert_eq!(together_finish.judgment, split_finish.judgment);
        assert_eq!(together_finish.resume_at, split_finish.resume_at);
    }

    #[test]
    fn environment_delivered_run_key_is_redacted_from_executor_events() {
        let mut request = request();
        request.assignment.run_key = None;
        let mut stream = ExecutorEventStream::new_with_secrets(&request, &["environment-run-key"]);
        let logs = stream
            .push(&line(json!({
                "version": 1,
                "type": "log",
                "stream": "stdout",
                "message": "credential=environment-run-key"
            })))
            .expect("parse event");
        assert_eq!(logs, ["credential=[REDACTED]\n"]);
    }

    fn request() -> ExecutionRequest {
        serde_json::from_value(json!({
            "version": 1,
            "tines": {"api_url": "https://tines.example.test"},
            "execution": {
                "harness": "codex",
                "workspace": {"parent": "/workspaces"},
                "retention": {"mode": "never", "max_age_hours": 72, "max_count": 20}
            },
            "assignment": {
                "run": {"id": "arun_test", "issue_id": "iss_test"},
                "prompt": "fixture",
                "bundle": {},
                "run_key": "issue-run-key",
                "timeout_minutes": 5,
                "env": [{"name": "SECRET", "value": "event-secret", "secret": true}]
            }
        }))
        .expect("decode test request")
    }

    fn line(value: Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&value).expect("serialize event");
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn parses_split_events_and_maps_latest_session_usage_pricing_and_rate_limit() {
        let mut stream = ExecutorEventStream::new(&request());
        let events = [
            json!({"version":1,"type":"log","stream":"stdout","message":"starting"}),
            json!({"version":1,"type":"session","provider":"codex","id":"thread-1"}),
            json!({"version":1,"type":"usage","input_tokens":17,"output_tokens":9}),
            json!({"version":1,"type":"rate_limit","resume_at":2_000_000_000_000_u64,"message":"try later"}),
            json!({
                "version":1,
                "type":"result",
                "status":"rate_limited",
                "exit_code":1,
                "error":"limited",
                "provider_session_id":"thread-2",
                "usage":{"input_tokens":20,"output_tokens":11},
                "rate_limit":{"resume_at":2_000_000_000_000_u64,"message":"try later"},
                "interrupted":false,
                "pricing_evidence":{
                    "provider":"codex",
                    "version":1,
                    "payload":{
                        "version":1,
                        "harness":"codex",
                        "model":"gpt-5.1-codex",
                        "identity_source":"launch_argument",
                        "usage_scope":"thread_total",
                        "session_mode":"cold",
                        "normalization":"codex-jsonl-v1",
                        "model_rerouted":false,
                        "measurement_status":"complete",
                        "terminal_snapshots":1,
                        "request_context":{
                            "version":1,
                            "normalization":"codex-rollout-delta-v1",
                            "status":"unavailable",
                            "reason":"not_applicable"
                        }
                    }
                }
            }),
        ];
        let source = events.into_iter().flat_map(line).collect::<Vec<_>>();
        let logs = source
            .chunks(23)
            .flat_map(|chunk| stream.push(chunk).expect("parse protocol chunk"))
            .collect::<Vec<_>>();
        stream.finish().expect("valid terminal result");
        assert_eq!(logs, ["starting\n"]);

        let finish = stream.finish_request(None, "", false);
        assert_eq!(finish.status, FinishStatus::Failed);
        assert_eq!(finish.judgment, Some(FinishJudgment::RateLimited));
        assert_eq!(finish.resume_at, Some(2_000_000_000_000));
        assert_eq!(finish.provider_session_id.as_deref(), Some("thread-2"));
        assert_eq!(finish.usage.unwrap().input_tokens, Some(20));
        let pricing = finish.pricing_evidence.unwrap();
        assert_eq!(pricing.model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(
            pricing.request_context.as_ref().unwrap()["reason"],
            "not_applicable"
        );
    }

    #[test]
    fn protocol_failure_overrides_a_terminal_result_and_keeps_stderr_diagnostics() {
        let mut stream = ExecutorEventStream::new(&request());
        stream
            .push(&line(json!({"version":1,"type":"result","status":"completed","exit_code":0,"interrupted":false})))
            .expect("parse terminal result");
        let error = stream
            .push(&line(
                json!({"version":1,"type":"log","stream":"stdout","message":"after result"}),
            ))
            .expect_err("reject output after result");
        let finish = stream.finish_request(Some(&error.to_string()), "diagnostic", false);
        assert_eq!(finish.status, FinishStatus::Failed);
        assert!(
            finish
                .error
                .unwrap()
                .contains("Executor stderr:\ndiagnostic")
        );
    }

    #[test]
    fn redacts_secrets_from_logs_finish_data_and_pricing_payload() {
        let mut stream = ExecutorEventStream::new(&request());
        let mut pricing = json!({
            "provider":"codex",
            "version":1,
            "payload":{
                "version":1,
                "harness":"codex",
                "model":null,
                "identity_source":"launch_argument",
                "usage_scope":"thread_total",
                "session_mode":"cold",
                "normalization":"codex-jsonl-v1",
                "model_rerouted":false,
                "measurement_status":"missing",
                "terminal_snapshots":0,
                "extra":"issue-run-key"
            }
        });
        // Keep an unknown provider payload opaque here so the redactor sees it
        // before the Tines Codex pricing adapter drops it.
        pricing["provider"] = json!("future-provider");
        let events = [
            json!({"version":1,"type":"log","stream":"stdout","message":"issue-run-key event-secret"}),
            json!({
                "version":1,
                "type":"result",
                "status":"failed",
                "exit_code":1,
                "error":"event-secret",
                "pricing_evidence":pricing,
                "interrupted":false
            }),
        ];
        let source = events.into_iter().flat_map(line).collect::<Vec<_>>();
        let logs = stream.push(&source).expect("parse events");
        stream.finish().expect("terminal result");
        assert_eq!(logs, ["[REDACTED] [REDACTED]\n"]);
        let finish = stream.finish_request(None, "event-secret", false);
        let serialized = serde_json::to_string(&finish).expect("serialize safe finish");
        assert!(!serialized.contains("issue-run-key"));
        assert!(!serialized.contains("event-secret"));
        assert_eq!(finish.status, FinishStatus::Failed);
    }

    #[test]
    fn completed_failed_interrupted_and_rate_limited_results_map_to_tines() {
        let cases = [
            (
                json!({"version":1,"type":"result","status":"completed","exit_code":0,"interrupted":false}),
                FinishStatus::Completed,
                None,
            ),
            (
                json!({"version":1,"type":"result","status":"failed","exit_code":1,"error":"failed","interrupted":false}),
                FinishStatus::Failed,
                None,
            ),
            (
                json!({"version":1,"type":"result","status":"failed","exit_code":1,"error":"interrupted","interrupted":true}),
                FinishStatus::Failed,
                Some(FinishJudgment::Interrupted),
            ),
            (
                json!({"version":1,"type":"result","status":"rate_limited","exit_code":1,"error":"limited","rate_limit":{"message":"wait"},"interrupted":false}),
                FinishStatus::Failed,
                Some(FinishJudgment::RateLimited),
            ),
        ];

        for (event, expected_status, expected_judgment) in cases {
            let mut stream = ExecutorEventStream::new(&request());
            stream.push(&line(event)).expect("parse result");
            stream.finish().expect("valid result");
            let finish = stream.finish_request(None, "", false);
            assert_eq!(finish.status, expected_status);
            assert_eq!(finish.judgment, expected_judgment);
        }
    }
}
