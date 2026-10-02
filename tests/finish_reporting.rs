use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{Local, NaiveDateTime, TimeZone};
use tines_runner_rs::codex_stream::CodexStreamParser;
use tines_runner_rs::finish::CodexRunReport;
use tines_runner_rs::protocol::client::Client;
use tines_runner_rs::protocol::{FinishRunRequest, FinishStatus};

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let count = stream.read(&mut chunk).expect("read request");
        assert_ne!(count, 0, "client closed before completing request");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("request headers");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            request.truncate(header_end + 4 + content_length);
            return String::from_utf8(request).expect("request is UTF-8");
        }
    }
}

fn finish_server(count: usize) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock server address");
    let server = thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (mut stream, _) = listener.accept().expect("accept finish request");
                let request = read_request(&mut stream);
                let body = if request.contains("/arun_completed/finish ") {
                    r#"{"id":"arun_completed","status":"completed"}"#
                } else if request.contains("/arun_limited/finish ") {
                    r#"{"id":"arun_limited","status":"failed"}"#
                } else {
                    r#"{"id":"arun_failed","status":"failed"}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write finish response");
                request
            })
            .collect()
    });
    (format!("http://{address}"), server)
}

fn report_from_fixture(status: FinishStatus, error: Option<&str>) -> FinishRunRequest {
    report_from_stream(include_str!("fixtures/codex-stream.jsonl"), status, error)
}

fn report_from_stream(stream: &str, status: FinishStatus, error: Option<&str>) -> FinishRunRequest {
    let mut parser = CodexStreamParser::default();
    let mut report = CodexRunReport::new(Some("gpt-5.1-codex"));
    for chunk in stream.as_bytes().chunks(19) {
        let chunk = std::str::from_utf8(chunk).expect("fixture chunk is valid UTF-8");
        for event in parser.push(chunk) {
            report.observe(&event);
        }
    }
    for event in parser.finish() {
        report.observe(&event);
    }
    report.into_finish_request(status, error.map(str::to_owned))
}

fn request_body(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("HTTP request body");
    serde_json::from_str(body).expect("JSON finish request")
}

#[test]
fn fake_server_receives_completed_and_failed_finish_payloads_with_observed_usage() {
    let (server_url, server) = finish_server(3);
    let client = Client::with_timeout(&server_url, Duration::from_secs(5))
        .expect("create runner protocol client");

    client
        .finish_run(
            "arun_completed",
            "runner-token",
            &report_from_fixture(FinishStatus::Completed, None),
        )
        .expect("report successful run");
    client
        .finish_run(
            "arun_failed",
            "runner-token",
            &report_from_fixture(FinishStatus::Failed, Some("harness exited with code 1")),
        )
        .expect("report failed run");
    client
        .finish_run(
            "arun_limited",
            "runner-token",
            &report_from_stream(
                include_str!("fixtures/codex-usage-limit.jsonl"),
                FinishStatus::Failed,
                Some("Codex exited with code 1"),
            ),
        )
        .expect("report rate-limited run");

    let requests = server.join().expect("join fake Tines server");
    let completed = request_body(&requests[0]);
    let failed = request_body(&requests[1]);
    let limited = request_body(&requests[2]);

    assert!(requests[0].starts_with("POST /api/v1/runs/arun_completed/finish "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_failed/finish "));
    assert!(requests[2].starts_with("POST /api/v1/runs/arun_limited/finish "));
    for request in &requests {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer runner-token")
        );
    }

    assert_eq!(completed["status"], "completed");
    assert!(completed.get("error").is_none());
    assert_eq!(
        completed["provider_session_id"],
        "01a09e68-24d8-78d3-8bc7-67037a0cd7de"
    );
    assert_eq!(
        completed["usage"],
        serde_json::json!({
            "input_tokens": 8913,
            "output_tokens": 1980,
            "cache_read_tokens": 101888,
            "cache_write_tokens": 0
        })
    );
    assert_eq!(completed["pricing_evidence"]["version"], 1);
    assert_eq!(completed["pricing_evidence"]["harness"], "codex");
    assert_eq!(completed["pricing_evidence"]["model"], "gpt-5.1-codex");
    assert_eq!(
        completed["pricing_evidence"]["identity_source"],
        "launch_argument"
    );
    assert_eq!(completed["pricing_evidence"]["usage_scope"], "thread_total");
    assert_eq!(completed["pricing_evidence"]["session_mode"], "cold");
    assert_eq!(
        completed["pricing_evidence"]["normalization"],
        "codex-jsonl-v1"
    );
    assert_eq!(
        completed["pricing_evidence"]["measurement_status"],
        "complete"
    );
    assert_eq!(completed["pricing_evidence"]["terminal_snapshots"], 1);
    assert_eq!(
        completed["pricing_evidence"]["raw_usage"],
        serde_json::json!({
            "input_tokens": 110801,
            "cached_input_tokens": 101888,
            "cache_write_input_tokens": 0,
            "output_tokens": 1980
        })
    );

    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"], "harness exited with code 1");
    assert!(failed.get("judgment").is_none());
    assert!(failed.get("resume_at").is_none());
    assert_eq!(
        failed["provider_session_id"],
        completed["provider_session_id"]
    );
    assert_eq!(failed["usage"], completed["usage"]);
    assert_eq!(
        failed["pricing_evidence"]["raw_usage"],
        completed["pricing_evidence"]["raw_usage"]
    );
    assert_eq!(
        failed["pricing_evidence"]["measurement_status"],
        "incomplete_attempt"
    );

    assert_eq!(limited["status"], "failed");
    assert_eq!(limited["judgment"], "rate_limited");
    assert_eq!(limited["resume_at"], 1_791_000_000_000_u64);
    assert_eq!(limited["provider_session_id"], "thread-limited");
    assert_eq!(
        limited["usage"],
        serde_json::json!({
            "input_tokens": 95,
            "output_tokens": 8,
            "cache_read_tokens": 20,
            "cache_write_tokens": 5
        })
    );
}

#[test]
fn structured_usage_limit_keeps_observed_session_and_usage_and_reports_reset() {
    let request = report_from_stream(
        include_str!("fixtures/codex-usage-limit.jsonl"),
        FinishStatus::Failed,
        Some("Codex exited with code 1"),
    );
    let value = serde_json::to_value(request).expect("serialize finish request");

    assert_eq!(value["status"], "failed");
    assert_eq!(value["judgment"], "rate_limited");
    assert_eq!(value["resume_at"], 1_791_000_000_000_u64);
    assert_eq!(value["provider_session_id"], "thread-limited");
    assert_eq!(
        value["usage"],
        serde_json::json!({
            "input_tokens": 95,
            "output_tokens": 8,
            "cache_read_tokens": 20,
            "cache_write_tokens": 5
        })
    );
}

#[test]
fn recognized_codex_rate_limit_message_without_code_gets_no_guessed_reset() {
    let request = report_from_stream(
        include_str!("fixtures/codex-rate-limit-message.jsonl"),
        FinishStatus::Failed,
        None,
    );
    let value = serde_json::to_value(request).expect("serialize finish request");

    assert_eq!(value["judgment"], "rate_limited");
    assert!(value.get("resume_at").is_none());
}

#[test]
fn structured_rate_limit_code_parses_reset_with_offset() {
    let request = report_from_stream(
        include_str!("fixtures/codex-rate-limit.jsonl"),
        FinishStatus::Failed,
        Some("Codex exited with code 1"),
    );
    let value = serde_json::to_value(request).expect("serialize finish request");

    assert_eq!(value["judgment"], "rate_limited");
    assert_eq!(value["resume_at"], 1_791_075_723_000_u64);
}

#[test]
fn recognized_usage_limit_message_parses_codex_local_reset_time() {
    let request = report_from_stream(
        include_str!("fixtures/codex-local-reset-message.jsonl"),
        FinishStatus::Failed,
        None,
    );
    let value = serde_json::to_value(request).expect("serialize finish request");
    let reset = NaiveDateTime::parse_from_str("Oct 4 2026 11:02 AM", "%b %d %Y %I:%M %p")
        .expect("fixture reset time");
    let expected = Local
        .from_local_datetime(&reset)
        .single()
        .expect("unambiguous fixture reset time")
        .timestamp_millis() as u64;

    assert_eq!(value["judgment"], "rate_limited");
    assert_eq!(value["resume_at"], expected);
}

#[test]
fn authentication_and_model_errors_do_not_become_rate_limits() {
    let request = report_from_stream(
        include_str!("fixtures/codex-provider-errors.jsonl"),
        FinishStatus::Failed,
        Some("Codex exited with code 1"),
    );
    let value = serde_json::to_value(request).expect("serialize finish request");

    assert!(value.get("judgment").is_none());
    assert!(value.get("resume_at").is_none());
}
