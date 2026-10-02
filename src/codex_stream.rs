//! Incremental parsing and readable rendering for `codex exec --json` output.

use chrono::{DateTime, Local, LocalResult, NaiveDateTime, NaiveTime, TimeZone};
use serde_json::Value;

const MAX_SAFE_EPOCH_MS: u64 = 9_007_199_254_740_991;

/// A provider limit reported by a terminal Codex error event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexRateLimit {
    /// Provider reset instant as epoch milliseconds, when available.
    pub resume_at: Option<u64>,
    /// The terminal provider message, when present.
    pub message: Option<String>,
}

/// A structured Codex event or a non-JSON line from the child process.
///
/// Structured events retain the complete original JSON value. Callers can use
/// [`CodexEvent::raw_usage`] and [`CodexEvent::raw`] to derive terminal usage
/// or other evidence without losing additive fields this version does not know.
#[derive(Clone, Debug, PartialEq)]
pub enum CodexEvent {
    Structured(Value),
    Malformed(String),
}

impl CodexEvent {
    /// Return the Codex event type when this record is a JSON object.
    pub fn event_type(&self) -> Option<&str> {
        self.raw()?.get("type")?.as_str()
    }

    /// Return the complete parsed JSON value for a structured event.
    pub fn raw(&self) -> Option<&Value> {
        match self {
            Self::Structured(value) => Some(value),
            Self::Malformed(_) => None,
        }
    }

    /// Return the provider thread identifier from `thread.started`.
    pub fn thread_id(&self) -> Option<&str> {
        if self.event_type()? != "thread.started" {
            return None;
        }
        self.raw()?.get("thread_id")?.as_str()
    }

    /// Return the original cumulative usage object from `turn.completed`.
    pub fn raw_usage(&self) -> Option<&Value> {
        if self.event_type()? != "turn.completed" {
            return None;
        }
        self.raw()?.get("usage")
    }

    /// Return a supported provider usage-limit signal from a terminal event.
    ///
    /// Classification is limited to top-level `turn.failed` and `error`
    /// events. Item errors and assistant text can describe a limit without
    /// meaning that the provider rejected the run.
    pub fn rate_limit(&self) -> Option<CodexRateLimit> {
        let raw = self.raw()?;
        let event_type = self.event_type()?;
        let error = match event_type {
            "turn.failed" => raw.get("error")?,
            "error" => raw,
            _ => return None,
        };

        let code = ["codex_error_info", "code"]
            .into_iter()
            .find_map(|key| error.get(key).and_then(Value::as_str));
        let code = code.or_else(|| {
            (event_type == "turn.failed")
                .then(|| error.get("type").and_then(Value::as_str))
                .flatten()
        });
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty());

        let supported_code = match code {
            Some("usage_limit_exceeded" | "rate_limit_exceeded" | "quota_exceeded") => {
                !message.is_some_and(is_plan_entitlement_message)
            }
            Some(_) => false,
            None => message.is_some_and(is_supported_limit_message),
        };
        if !supported_code {
            return None;
        }

        Some(CodexRateLimit {
            resume_at: reset_timestamp(error)
                .or_else(|| message.and_then(parse_timestamp_from_message)),
            message: message.map(str::to_owned),
        })
    }

    /// Render this event as zero or more concise run-log lines.
    ///
    /// Unknown structured event types are ignored. Their complete JSON value
    /// remains available through [`CodexEvent::raw`].
    pub fn render_lines(&self) -> Vec<String> {
        let Self::Structured(raw) = self else {
            let Self::Malformed(line) = self else {
                unreachable!()
            };
            return vec![clip(line.trim(), 2_000)];
        };

        let event_type = raw.get("type").and_then(Value::as_str);
        match event_type {
            Some("thread.started") => {
                let thread = raw.get("thread_id").and_then(Value::as_str);
                vec![match thread {
                    Some(thread) if !thread.trim().is_empty() => {
                        format!("[session] started (thread {})", clip(thread, 200))
                    }
                    _ => "[session] started".to_owned(),
                }]
            }
            Some("turn.started") => vec!["[session] turn started".to_owned()],
            Some("turn.completed") => vec!["[session] turn completed".to_owned()],
            Some("turn.failed") => vec![format!(
                "[error] {}",
                error_message(raw).unwrap_or_else(|| "turn failed".to_owned())
            )],
            Some("error") => vec![format!(
                "[error] {}",
                error_message(raw).unwrap_or_else(|| "unknown error".to_owned())
            )],
            Some("item.started") | Some("item.completed") => {
                render_item(event_type.unwrap_or_default(), raw.get("item"))
            }
            _ => Vec::new(),
        }
    }
}

fn is_plan_entitlement_message(message: &str) -> bool {
    message
        .trim_start()
        .starts_with("To use Codex with your ChatGPT plan, upgrade to Plus:")
}

fn is_supported_limit_message(message: &str) -> bool {
    let message = message.trim_start();
    if message
        .get(.."rate limit exceeded:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("rate limit exceeded:"))
    {
        return true;
    }
    [
        "You've hit your usage limit",
        "You’ve hit your usage limit",
        "Quota exceeded. Check your plan and billing details.",
        "Your workspace is out of credits.",
        "You hit your spend cap set in your workspace.",
        "You hit your spend cap set by the owner of your workspace.",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

fn reset_timestamp(error: &Value) -> Option<u64> {
    for key in [
        "resets_at",
        "reset_at",
        "retry_at",
        "resetsAt",
        "resetAt",
        "retryAt",
    ] {
        if let Some(timestamp) = error.get(key).and_then(parse_timestamp_value) {
            return Some(timestamp);
        }
    }
    error
        .get("rate_limit_info")
        .and_then(|info| info.get("resetsAt"))
        .and_then(parse_timestamp_value)
}

fn parse_timestamp_value(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return normalize_epoch(number);
    }
    let value = value.as_str()?.trim();
    value
        .parse::<u64>()
        .ok()
        .and_then(normalize_epoch)
        .or_else(|| parse_rfc3339(value))
}

fn normalize_epoch(value: u64) -> Option<u64> {
    if value == 0 {
        None
    } else if value < 1_000_000_000_000 {
        value
            .checked_mul(1_000)
            .filter(|milliseconds| *milliseconds <= MAX_SAFE_EPOCH_MS)
    } else {
        (value <= MAX_SAFE_EPOCH_MS).then_some(value)
    }
}

fn parse_timestamp_from_message(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    let start = lower.find("try again at ")? + "try again at ".len();
    let value = message[start..]
        .split_once('\n')
        .map_or(&message[start..], |(line, _)| line)
        .trim()
        .trim_end_matches(|character: char| {
            matches!(character, '.' | ',' | ')' | ']' | '"' | '\'')
        });
    parse_rfc3339(value).or_else(|| parse_codex_local_reset(value))
}

fn parse_rfc3339(value: &str) -> Option<u64> {
    u64::try_from(DateTime::parse_from_rfc3339(value).ok()?.timestamp_millis())
        .ok()
        .filter(|milliseconds| *milliseconds <= MAX_SAFE_EPOCH_MS)
}

fn parse_codex_local_reset(value: &str) -> Option<u64> {
    if let Some((date, time)) = value.split_once(", ") {
        let (month, day) = date.split_once(' ')?;
        let day = ["st", "nd", "rd", "th"]
            .iter()
            .find_map(|suffix| day.strip_suffix(suffix))
            .unwrap_or(day);
        let (year, time) = time.split_once(' ')?;
        let date_time = format!("{month} {day} {year} {time}");
        let parsed = NaiveDateTime::parse_from_str(&date_time, "%b %d %Y %I:%M %p")
            .or_else(|_| NaiveDateTime::parse_from_str(&date_time, "%b %d %Y %I %p"))
            .ok()?;
        return local_epoch_millis(parsed);
    }

    let time = NaiveTime::parse_from_str(value, "%I:%M %p")
        .or_else(|_| NaiveTime::parse_from_str(value, "%I %p"))
        .ok()?;
    local_epoch_millis(Local::now().date_naive().and_time(time))
}

fn local_epoch_millis(value: NaiveDateTime) -> Option<u64> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(value) => u64::try_from(value.timestamp_millis())
            .ok()
            .filter(|milliseconds| *milliseconds <= MAX_SAFE_EPOCH_MS),
        LocalResult::Ambiguous(_, _) | LocalResult::None => None,
    }
}

/// Incrementally splits and parses Codex JSONL from arbitrary text chunks.
#[derive(Clone, Debug, Default)]
pub struct CodexStreamParser {
    pending: String,
    finished: bool,
}

impl CodexStreamParser {
    /// Add a child-process output chunk and return each complete record.
    pub fn push(&mut self, chunk: &str) -> Vec<CodexEvent> {
        if self.finished {
            return Vec::new();
        }

        self.pending.push_str(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.pending[start..].find('\n') {
            let end = start + offset;
            if let Some(event) = parse_line(&self.pending[start..end]) {
                events.push(event);
            }
            start = end + 1;
        }
        if start > 0 {
            self.pending.drain(..start);
        }
        events
    }

    /// Parse a final unterminated record. Repeated calls return no records.
    pub fn finish(&mut self) -> Vec<CodexEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let pending = std::mem::take(&mut self.pending);
        parse_line(&pending).into_iter().collect()
    }
}

fn parse_line(line: &str) -> Option<CodexEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(match serde_json::from_str(trimmed) {
        Ok(value) => CodexEvent::Structured(value),
        Err(_) => CodexEvent::Malformed(trimmed.to_owned()),
    })
}

fn error_message(event: &Value) -> Option<String> {
    let message = event
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| event.get("message").and_then(Value::as_str))
        .filter(|message| !message.trim().is_empty())?;
    Some(clip(message, 2_000))
}

fn render_item(event_type: &str, item: Option<&Value>) -> Vec<String> {
    let Some(item) = item.and_then(Value::as_object) else {
        return Vec::new();
    };
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") if event_type == "item.completed" => item
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| vec![format!("[agent] {}", clip(text, 2_000))])
            .unwrap_or_default(),
        Some("command_execution") => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .unwrap_or("command");
            if event_type == "item.started" {
                return vec![format!("[tool] running: {}", clip(command, 300))];
            }
            let exit_code = item.get("exit_code").and_then(Value::as_i64);
            let output = item
                .get("aggregated_output")
                .and_then(Value::as_str)
                .filter(|output| !output.trim().is_empty());
            let status = match (exit_code, output) {
                (Some(code), Some(output)) if code != 0 => {
                    format!(" (exit {code}: {})", clip(output, 300))
                }
                (Some(code), _) if code != 0 => format!(" (exit {code})"),
                _ => String::new(),
            };
            vec![format!("[tool] completed: {}{status}", clip(command, 300))]
        }
        Some("file_change") if event_type == "item.completed" => {
            let paths = item
                .get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|change| change.get("path").and_then(Value::as_str))
                .collect::<Vec<_>>();
            let suffix = if paths.is_empty() {
                String::new()
            } else {
                format!(": {}", clip(&paths.join(", "), 300))
            };
            vec![format!("[tool] file change{suffix}")]
        }
        Some("mcp_tool_call") if event_type == "item.completed" => {
            let name = ["server", "tool"]
                .into_iter()
                .filter_map(|key| item.get(key).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("/");
            vec![format!(
                "[tool] {}",
                clip(if name.is_empty() { "MCP call" } else { &name }, 300)
            )]
        }
        Some("web_search") if event_type == "item.completed" => {
            let query = item.get("query").and_then(Value::as_str);
            vec![match query {
                Some(query) if !query.trim().is_empty() => {
                    format!("[tool] web search: {}", clip(query, 300))
                }
                _ => "[tool] web search".to_owned(),
            }]
        }
        Some("error") if event_type == "item.completed" => {
            let message = item
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .map(|message| clip(message, 2_000))
                .unwrap_or_else(|| "unknown error".to_owned());
            vec![format!("[error] {message}")]
        }
        _ => Vec::new(),
    }
}

fn clip(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let clipped = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}
