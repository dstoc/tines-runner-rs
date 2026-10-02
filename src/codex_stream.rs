//! Incremental parsing and readable rendering for `codex exec --json` output.

use serde_json::Value;

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
