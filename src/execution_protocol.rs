//! Versioned JSON and JSONL types for one local executor invocation.

use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, de::Error as DeError};
use serde_json::{Map, Value};

use crate::config::RetentionMode;
use crate::protocol::RunnerAssignment;

/// The current daemon/executor protocol version.
pub const EXECUTION_PROTOCOL_VERSION: u16 = 1;

/// Maximum size in bytes of one executor event line, excluding its newline.
pub const MAX_EXECUTION_EVENT_LINE_BYTES: usize = 1024 * 1024;

/// The complete assignment and resolved local policy sent to one executor.
///
/// The request is serialized to executor stdin. Its run key and secret
/// environment values are intentionally present on the wire, but are omitted
/// from its Debug representation.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionRequest {
    #[serde(deserialize_with = "deserialize_supported_version")]
    pub version: u16,
    pub tines: TinesExecutionContext,
    pub execution: LocalExecutionPolicy,
    pub assignment: RunnerAssignment,
}

impl ExecutionRequest {
    /// Validate request fields that require semantic checks after JSON decoding.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != EXECUTION_PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(self.version));
        }
        let api_url = self
            .tines
            .api_url
            .parse::<url::Url>()
            .map_err(|_| ProtocolError::InvalidRequest)?;
        if !matches!(api_url.scheme(), "http" | "https")
            || api_url.host().is_none()
            || self.execution.harness.trim().is_empty()
            || self.execution.workspace.parent.as_os_str().is_empty()
        {
            return Err(ProtocolError::InvalidRequest);
        }
        Ok(())
    }

    /// Return the values that must be redacted from rendered executor events.
    fn secret_values(&self) -> Vec<&str> {
        let mut values = Vec::new();
        if !self.assignment.run_key.is_empty() {
            values.push(self.assignment.run_key.as_str());
        }
        values.extend(
            self.assignment
                .env
                .iter()
                .filter(|entry| entry.secret && !entry.value.is_empty())
                .map(|entry| entry.value.as_str()),
        );
        values
    }
}

/// Read and validate exactly one execution request without exposing parser
/// details that may contain assignment data.
pub fn read_execution_request<R: Read>(reader: R) -> Result<ExecutionRequest, RequestError> {
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let value = Value::deserialize(&mut deserializer).map_err(|_| RequestError::MalformedJson)?;
    deserializer
        .end()
        .map_err(|_| RequestError::MalformedJson)?;

    match value.get("version").and_then(Value::as_u64) {
        Some(version) if version != u64::from(EXECUTION_PROTOCOL_VERSION) => {
            return Err(RequestError::UnsupportedVersion(version));
        }
        Some(_) => {}
        None => return Err(RequestError::InvalidRequest),
    }

    let request = serde_json::from_value::<ExecutionRequest>(value)
        .map_err(|_| RequestError::InvalidRequest)?;
    request
        .validate()
        .map_err(|_| RequestError::InvalidRequest)?;
    Ok(request)
}

/// Safe, deterministic errors returned while reading an execution request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    MalformedJson,
    UnsupportedVersion(u64),
    InvalidRequest,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson => f.write_str("malformed execution request JSON"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported execution protocol version {version}")
            }
            Self::InvalidRequest => f.write_str("invalid executor request"),
        }
    }
}

impl std::error::Error for RequestError {}

impl fmt::Debug for ExecutionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut secrets = self.secret_values();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        f.debug_struct("ExecutionRequest")
            .field("version", &self.version)
            .field("tines", &RedactedMarker)
            .field(
                "execution",
                &RedactedExecution {
                    policy: &self.execution,
                    secrets: &secrets,
                },
            )
            .field(
                "assignment",
                &RedactedAssignment {
                    assignment: &self.assignment,
                    secrets: &secrets,
                },
            )
            .finish()
    }
}

struct RedactedMarker;

impl fmt::Debug for RedactedMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

struct RedactedDebugString<'a> {
    value: &'a str,
    secrets: &'a [&'a str],
}

impl fmt::Debug for RedactedDebugString<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rendered = self.value.to_owned();
        for secret in self.secrets {
            rendered = rendered.replace(secret, "[REDACTED]");
        }
        fmt::Debug::fmt(&rendered, f)
    }
}

struct RedactedExecution<'a> {
    policy: &'a LocalExecutionPolicy,
    secrets: &'a [&'a str],
}

impl fmt::Debug for RedactedExecution<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalExecutionPolicy")
            .field(
                "harness",
                &RedactedDebugString {
                    value: &self.policy.harness,
                    secrets: self.secrets,
                },
            )
            .field(
                "workspace",
                &RedactedWorkspace {
                    policy: &self.policy.workspace,
                    secrets: self.secrets,
                },
            )
            .field("retention", &self.policy.retention)
            .finish()
    }
}

struct RedactedWorkspace<'a> {
    policy: &'a WorkspacePolicy,
    secrets: &'a [&'a str],
}

impl fmt::Debug for RedactedWorkspace<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parent = self.policy.parent.to_string_lossy();
        f.debug_struct("WorkspacePolicy")
            .field(
                "parent",
                &RedactedDebugString {
                    value: &parent,
                    secrets: self.secrets,
                },
            )
            .finish()
    }
}

struct RedactedAssignment<'a> {
    assignment: &'a RunnerAssignment,
    secrets: &'a [&'a str],
}

impl fmt::Debug for RedactedAssignment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let assignment = self.assignment;
        f.debug_struct("Assignment")
            .field(
                "run_id",
                &RedactedDebugString {
                    value: &assignment.run.id,
                    secrets: self.secrets,
                },
            )
            .field(
                "issue_id",
                &RedactedDebugString {
                    value: &assignment.run.issue_id,
                    secrets: self.secrets,
                },
            )
            .field("run_key", &"[REDACTED]")
            .field("prompt", &"[REDACTED]")
            .field("bundle", &"[REDACTED]")
            .field("env_entries", &assignment.env.len())
            .field("timeout_minutes", &assignment.timeout_minutes)
            .finish()
    }
}

/// Tines connection details needed by the executor for assignment-scoped API
/// calls. The long-lived runner token is deliberately absent.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TinesExecutionContext {
    pub api_url: String,
}

/// Execution policy resolved by the daemon for this assignment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalExecutionPolicy {
    /// Semantic harness name, such as `codex`; this is not an executable path.
    pub harness: String,
    pub workspace: WorkspacePolicy,
    pub retention: ExecutionRetentionPolicy,
}

/// Local workspace location passed to the executor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePolicy {
    pub parent: PathBuf,
}

/// Retention settings in stable, platform-independent units.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionRetentionPolicy {
    pub mode: RetentionMode,
    pub max_age_hours: u64,
    pub max_count: usize,
}

/// Stream name attached to a normalized log event.
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
    System,
}

/// Terminal classification used by the executor protocol.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Completed,
    Failed,
    RateLimited,
}

/// Harness-neutral token usage. Unknown usage dimensions can be omitted.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl ExecutionUsage {
    fn has_value(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_tokens.is_some()
    }
}

/// Provider rate-limit details. `resume_at` uses Unix epoch milliseconds,
/// matching Tines finish reporting.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionRateLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Opaque provider evidence. The daemon carries its payload to Tines without
/// making the local protocol depend on a particular provider's wire format.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionPricingEvidence {
    pub provider: String,
    pub version: u32,
    pub payload: Value,
}

/// All terminal data required to settle the corresponding Tines run.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalResult {
    pub status: TerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExecutionUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_evidence: Option<ExecutionPricingEvidence>,
    #[serde(default)]
    pub interrupted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<ExecutionRateLimit>,
}

impl TerminalResult {
    /// Check the terminal result invariants required by protocol v1.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let valid = match self.status {
            TerminalStatus::Completed => {
                self.exit_code == Some(0)
                    && self.error.is_none()
                    && !self.interrupted
                    && self.rate_limit.is_none()
            }
            TerminalStatus::Failed => {
                self.exit_code != Some(0)
                    && (self.exit_code.is_some()
                        || self.error.is_some()
                        || self.interrupted
                        || self.rate_limit.is_some())
            }
            TerminalStatus::RateLimited => {
                self.rate_limit.is_some() && self.exit_code != Some(0) && !self.interrupted
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ProtocolError::InvalidTerminalResult)
        }
    }
}

/// One normalized executor event. Unknown event types are ignored by the
/// parser when they use the current protocol version.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionEvent {
    #[serde(deserialize_with = "deserialize_supported_version")]
    pub version: u16,
    #[serde(flatten)]
    pub kind: ExecutionEventKind,
}

impl ExecutionEvent {
    /// Construct an event using the current protocol version.
    pub fn new(kind: ExecutionEventKind) -> Self {
        Self {
            version: EXECUTION_PROTOCOL_VERSION,
            kind,
        }
    }

    /// Validate a typed event before rendering or handling it.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != EXECUTION_PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(self.version));
        }
        match &self.kind {
            ExecutionEventKind::Log { .. } => {}
            ExecutionEventKind::Session { provider, id } => {
                if provider.trim().is_empty() || id.trim().is_empty() {
                    return Err(ProtocolError::InvalidEvent);
                }
            }
            ExecutionEventKind::ProviderError {
                provider,
                code,
                message,
            } => {
                if provider.trim().is_empty()
                    || code.as_ref().is_some_and(|value| value.trim().is_empty())
                    || message.trim().is_empty()
                {
                    return Err(ProtocolError::InvalidEvent);
                }
            }
            ExecutionEventKind::Usage { usage } => {
                if !usage.has_value() {
                    return Err(ProtocolError::InvalidEvent);
                }
            }
            ExecutionEventKind::RateLimit { rate_limit } => {
                if rate_limit.resume_at.is_none()
                    && rate_limit
                        .message
                        .as_deref()
                        .is_none_or(|message| message.trim().is_empty())
                {
                    return Err(ProtocolError::InvalidEvent);
                }
                if rate_limit.resume_at == Some(0) {
                    return Err(ProtocolError::InvalidEvent);
                }
            }
            ExecutionEventKind::Result { result } => result.validate()?,
        }
        Ok(())
    }

    fn is_terminal(&self) -> bool {
        matches!(self.kind, ExecutionEventKind::Result { .. })
    }
}

impl fmt::Debug for ExecutionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExecutionEvent { .. }")
    }
}

/// Known event payloads in protocol v1.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEventKind {
    Log {
        stream: LogStream,
        message: String,
    },
    Session {
        provider: String,
        id: String,
    },
    ProviderError {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
    Usage {
        #[serde(flatten)]
        usage: ExecutionUsage,
    },
    RateLimit {
        #[serde(flatten)]
        rate_limit: ExecutionRateLimit,
    },
    Result {
        #[serde(flatten)]
        result: TerminalResult,
    },
}

/// Render an event as one compact JSONL line after redacting request secrets.
///
/// The redactor replaces run keys and secret environment values in every
/// string value and object key, including opaque pricing evidence.
pub fn render_event_jsonl(
    event: &ExecutionEvent,
    request: &ExecutionRequest,
) -> Result<String, ProtocolError> {
    request.validate()?;
    event.validate()?;
    let mut value = serde_json::to_value(event).map_err(|_| ProtocolError::Serialization)?;
    let mut secrets = request
        .secret_values()
        .into_iter()
        .flat_map(|secret| {
            let json_escaped =
                serde_json::to_string(secret).expect("a Rust string always serializes to JSON");
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
    redact_value(&mut value, &secrets);
    let mut line = serde_json::to_string(&value).map_err(|_| ProtocolError::Serialization)?;
    if line.len() > MAX_EXECUTION_EVENT_LINE_BYTES {
        return Err(ProtocolError::OversizedLine {
            line: 1,
            max_bytes: MAX_EXECUTION_EVENT_LINE_BYTES,
        });
    }
    line.push('\n');
    Ok(line)
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                *text = text.replace(secret, "***");
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, secrets);
            }
        }
        Value::Object(values) => {
            let mut redacted = Map::new();
            for (mut key, mut value) in std::mem::take(values) {
                for secret in secrets {
                    key = key.replace(secret, "***");
                }
                redact_value(&mut value, secrets);
                redacted.insert(key, value);
            }
            *values = redacted;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Incremental, bounded parser for executor stdout JSONL.
#[derive(Default)]
pub struct ExecutionEventParser {
    pending: Vec<u8>,
    line: usize,
    terminal_seen: bool,
    finished: bool,
}

impl ExecutionEventParser {
    /// Add an arbitrary byte chunk and return recognized protocol events.
    /// Unknown additive event types are skipped.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ExecutionEvent>, ProtocolError> {
        if self.finished {
            return Err(ProtocolError::StreamAlreadyFinished);
        }

        let mut events = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                self.line += 1;
                if self.terminal_seen {
                    return Err(self.trailing_output_error());
                }
                let line = std::mem::take(&mut self.pending);
                if let Some(event) = self.parse_line(&line)? {
                    self.terminal_seen = event.is_terminal();
                    events.push(event);
                }
            } else {
                if self.pending.len() == MAX_EXECUTION_EVENT_LINE_BYTES {
                    return Err(ProtocolError::OversizedLine {
                        line: self.line + 1,
                        max_bytes: MAX_EXECUTION_EVENT_LINE_BYTES,
                    });
                }
                self.pending.push(*byte);
            }
        }
        Ok(events)
    }

    /// Finish the stream, parsing an optional final unterminated line and
    /// requiring exactly one terminal result.
    pub fn finish(&mut self) -> Result<Option<ExecutionEvent>, ProtocolError> {
        if self.finished {
            return Err(ProtocolError::StreamAlreadyFinished);
        }
        self.finished = true;

        if !self.pending.is_empty() {
            if self.terminal_seen {
                self.line += 1;
                return Err(self.trailing_output_error());
            }
            self.line += 1;
            let line = std::mem::take(&mut self.pending);
            if let Some(event) = self.parse_line(&line)? {
                self.terminal_seen = event.is_terminal();
                if !self.terminal_seen {
                    return Err(ProtocolError::MissingTerminalResult);
                }
                return Ok(Some(event));
            }
        }

        if self.terminal_seen {
            Ok(None)
        } else {
            Err(ProtocolError::MissingTerminalResult)
        }
    }

    fn parse_line(&self, bytes: &[u8]) -> Result<Option<ExecutionEvent>, ProtocolError> {
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|_| ProtocolError::MalformedJson { line: self.line })?;
        let object = value
            .as_object()
            .ok_or(ProtocolError::MalformedJson { line: self.line })?;
        let version = object
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(ProtocolError::MalformedJson { line: self.line })?;
        if version != u64::from(EXECUTION_PROTOCOL_VERSION) {
            return Err(ProtocolError::UnsupportedVersionValue {
                line: self.line,
                version,
            });
        }
        let event_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MalformedJson { line: self.line })?;
        if !matches!(
            event_type,
            "log" | "session" | "provider_error" | "usage" | "rate_limit" | "result"
        ) {
            return Ok(None);
        }
        let event: ExecutionEvent =
            serde_json::from_value(value).map_err(|_| ProtocolError::InvalidEvent)?;
        event.validate()?;
        Ok(Some(event))
    }

    fn trailing_output_error(&self) -> ProtocolError {
        let bytes = self.pending.strip_suffix(b"\r").unwrap_or(&self.pending);
        let is_result = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|event_type| event_type == "result");
        if is_result {
            ProtocolError::DuplicateTerminalResult { line: self.line }
        } else {
            ProtocolError::OutputAfterTerminal { line: self.line }
        }
    }
}

/// Errors that can occur while decoding or rendering the local protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    MalformedJson { line: usize },
    UnsupportedVersion(u16),
    UnsupportedVersionValue { line: usize, version: u64 },
    InvalidEvent,
    InvalidRequest,
    InvalidTerminalResult,
    OversizedLine { line: usize, max_bytes: usize },
    DuplicateTerminalResult { line: usize },
    OutputAfterTerminal { line: usize },
    MissingTerminalResult,
    StreamAlreadyFinished,
    Serialization,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson { line } => write!(f, "malformed executor JSONL at line {line}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported execution protocol version {version}")
            }
            Self::UnsupportedVersionValue { line, version } => write!(
                f,
                "unsupported execution protocol version {version} at line {line}"
            ),
            Self::InvalidEvent => f.write_str("invalid executor event"),
            Self::InvalidRequest => f.write_str("invalid executor request"),
            Self::InvalidTerminalResult => f.write_str("invalid executor terminal result"),
            Self::OversizedLine { line, max_bytes } => write!(
                f,
                "executor JSONL line {line} exceeds the {max_bytes}-byte limit"
            ),
            Self::DuplicateTerminalResult { line } => {
                write!(f, "duplicate executor terminal result at line {line}")
            }
            Self::OutputAfterTerminal { line } => {
                write!(f, "executor output after terminal result at line {line}")
            }
            Self::MissingTerminalResult => {
                f.write_str("executor stream ended without a terminal result")
            }
            Self::StreamAlreadyFinished => f.write_str("executor event stream is already finished"),
            Self::Serialization => f.write_str("could not serialize executor event"),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn deserialize_supported_version<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u16::deserialize(deserializer)?;
    if version == EXECUTION_PROTOCOL_VERSION {
        Ok(version)
    } else {
        Err(D::Error::custom(format!(
            "unsupported execution protocol version {version}"
        )))
    }
}
