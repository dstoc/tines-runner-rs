use std::fs;
use std::path::Path;

use tines_runner_rs::executor::codex_stream::{CodexEvent, CodexStreamParser};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    fs::read_to_string(path).expect("read Codex fixture")
}

#[test]
fn parses_and_renders_representative_jsonl_deterministically() {
    let input = fixture("codex-stream.jsonl");
    let mut parser = CodexStreamParser::default();
    let mut events = Vec::new();

    // Split event records across writes, including in the middle of JSON keys.
    for chunk in input.as_bytes().chunks(17) {
        let chunk = std::str::from_utf8(chunk).expect("fixture chunk is valid UTF-8");
        events.extend(parser.push(chunk));
    }
    events.extend(parser.finish());

    let lines = events
        .iter()
        .flat_map(CodexEvent::render_lines)
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            "[session] started (thread 01a09e68-24d8-78d3-8bc7-67037a0cd7de)",
            "[session] turn started",
            "[tool] running: cargo test",
            "[tool] completed: cargo test",
            "[agent] Patch complete.",
            "[tool] docs/search",
            "[session] turn completed",
        ]
    );

    let started = events
        .iter()
        .find(|event| event.event_type() == Some("thread.started"))
        .expect("thread event");
    assert_eq!(
        started.thread_id(),
        Some("01a09e68-24d8-78d3-8bc7-67037a0cd7de")
    );

    let terminal = events
        .iter()
        .find(|event| event.event_type() == Some("turn.completed"))
        .expect("terminal event");
    assert_eq!(
        terminal.raw_usage(),
        Some(&serde_json::json!({
            "input_tokens": 110801,
            "cached_input_tokens": 101888,
            "cache_write_input_tokens": 0,
            "output_tokens": 1980,
            "reasoning_output_tokens": 306
        }))
    );
    assert_eq!(
        terminal.raw().and_then(|raw| raw.get("future_metadata")),
        Some(&serde_json::json!({"source": "codex"}))
    );

    let unknown = events
        .iter()
        .find(|event| event.event_type() == Some("thread.metadata.updated"))
        .expect("unknown additive event is retained");
    assert!(unknown.render_lines().is_empty());
}

#[test]
fn renders_provider_errors_and_ignores_malformed_lines_without_stopping() {
    let input = fixture("codex-errors.jsonl");
    let mut parser = CodexStreamParser::default();
    let events = parser.push(&input);
    let lines = events
        .iter()
        .flat_map(CodexEvent::render_lines)
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            "[error] The provider stopped this turn.",
            "[error] Rate limit reached.",
            "[error] The selected model was unavailable.",
        ]
    );

    let mut parser = CodexStreamParser::default();
    let events = parser.push("not JSON\n{\"type\":\"turn.started\"}\n");
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], CodexEvent::Malformed(_)));
    assert_eq!(events[0].render_lines(), ["not JSON"]);
    assert_eq!(events[1].render_lines(), ["[session] turn started"]);
}

#[test]
fn finish_parses_an_unterminated_final_record_once() {
    let mut parser = CodexStreamParser::default();
    assert!(parser.push("{\"type\":\"turn.started\"}").is_empty());
    assert_eq!(parser.finish().len(), 1);
    assert!(parser.finish().is_empty());
    assert!(parser.push("{\"type\":\"turn.completed\"}\n").is_empty());
}
