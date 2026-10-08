//! Incremental parser for Antigravity's stream-JSON event output.

use serde_json::Value;

const MAX_LINE_BYTES: usize = 1024 * 1024;

/// One decoded Antigravity stream record.
#[derive(Clone, Debug, PartialEq)]
pub enum AntigravityEvent {
    Init(Value),
    StepUpdate(Value),
    Result(Value),
    Malformed,
    Other,
}

/// Incrementally splits bounded NDJSON records from arbitrary output chunks.
#[derive(Clone, Debug, Default)]
pub struct AntigravityStreamParser {
    pending: String,
    discard_line: bool,
    finished: bool,
}

impl AntigravityStreamParser {
    /// Add a process output chunk and return all complete records.
    pub fn push(&mut self, chunk: &str) -> Vec<AntigravityEvent> {
        if self.finished {
            return Vec::new();
        }
        let mut output = Vec::new();
        let mut remaining = chunk;
        loop {
            if self.discard_line {
                let Some(newline) = remaining.find('\n') else {
                    return output;
                };
                remaining = &remaining[newline + 1..];
                self.discard_line = false;
            }
            if let Some(newline) = remaining.find('\n') {
                let line = &remaining[..newline];
                if self.pending.len().saturating_add(line.len()) > MAX_LINE_BYTES {
                    output.push(AntigravityEvent::Malformed);
                    self.pending.clear();
                } else {
                    self.pending.push_str(line);
                    if let Some(event) = parse_line(&self.pending) {
                        output.push(event);
                    }
                    self.pending.clear();
                }
                remaining = &remaining[newline + 1..];
                if remaining.is_empty() {
                    return output;
                }
            } else {
                if self.pending.len().saturating_add(remaining.len()) > MAX_LINE_BYTES {
                    self.pending.clear();
                    self.discard_line = true;
                    output.push(AntigravityEvent::Malformed);
                } else {
                    self.pending.push_str(remaining);
                }
                return output;
            }
        }
    }

    /// Parse one final unterminated record. Repeated calls return no records.
    pub fn finish(&mut self) -> Vec<AntigravityEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.discard_line {
            self.pending.clear();
            return Vec::new();
        }
        let pending = std::mem::take(&mut self.pending);
        if pending.len() > MAX_LINE_BYTES {
            vec![AntigravityEvent::Malformed]
        } else {
            parse_line(&pending).into_iter().collect()
        }
    }
}

fn parse_line(line: &str) -> Option<AntigravityEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Some(AntigravityEvent::Malformed);
    };
    let Some(object) = value.as_object() else {
        return Some(AntigravityEvent::Malformed);
    };
    match object.get("event").and_then(Value::as_str) {
        Some("init") if object.get("init").is_some_and(Value::is_object) => {
            Some(AntigravityEvent::Init(value))
        }
        Some("step_update") if object.get("step_update").is_some_and(Value::is_object) => {
            Some(AntigravityEvent::StepUpdate(value))
        }
        Some("result") => match object.get("result") {
            Some(result)
                if result.is_object() && result.get("status").is_some_and(Value::is_string) =>
            {
                Some(AntigravityEvent::Result(value))
            }
            _ => Some(AntigravityEvent::Malformed),
        },
        Some("init" | "step_update") => Some(AntigravityEvent::Malformed),
        Some(_) => Some(AntigravityEvent::Other),
        None => Some(AntigravityEvent::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::{AntigravityEvent, AntigravityStreamParser};

    #[test]
    fn parses_incremental_init_step_result_and_unterminated_final_record() {
        let mut parser = AntigravityStreamParser::default();
        assert!(parser.push("{\"event\":\"ini").is_empty());
        let events = parser.push(
            "t\",\"conversation_id\":\"s1\",\"init\":{},\"future\":true}\n{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"future\"}}\n",
        );
        assert!(matches!(events[0], AntigravityEvent::Init(_)));
        assert!(matches!(events[1], AntigravityEvent::StepUpdate(_)));
        assert!(
            parser
                .push("{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}")
                .is_empty()
        );
        assert!(matches!(
            parser.finish().as_slice(),
            [AntigravityEvent::Result(_)]
        ));
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn ignores_unknown_events_and_reports_invalid_json_or_shapes() {
        let mut parser = AntigravityStreamParser::default();
        let events = parser.push(
            "{\"event\":\"future\",\"payload\":{}}\nnot-json\n[]\n{\"event\":\"result\",\"result\":{}}\n",
        );
        assert_eq!(
            events,
            [
                AntigravityEvent::Other,
                AntigravityEvent::Malformed,
                AntigravityEvent::Malformed,
                AntigravityEvent::Malformed,
            ]
        );
    }

    #[test]
    fn accepts_unknown_fields_and_future_step_types() {
        let mut parser = AntigravityStreamParser::default();
        let event = parser.push(
            "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"future_step\",\"extra\":7}}\n",
        );
        assert!(matches!(
            event.as_slice(),
            [AntigravityEvent::StepUpdate(_)]
        ));
    }

    #[test]
    fn oversized_records_are_discarded_until_the_next_line() {
        let mut parser = AntigravityStreamParser::default();
        let mut events = parser.push(&"x".repeat(1024 * 1024 + 1));
        events
            .extend(parser.push("\n{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\n"));
        assert!(matches!(
            events.as_slice(),
            [AntigravityEvent::Malformed, AntigravityEvent::Result(_)]
        ));
    }

    #[test]
    fn result_requires_a_terminal_status() {
        let mut parser = AntigravityStreamParser::default();
        assert_eq!(
            parser.push("{\"event\":\"result\",\"result\":{\"status\":null}}\n"),
            [AntigravityEvent::Malformed]
        );
    }
}
