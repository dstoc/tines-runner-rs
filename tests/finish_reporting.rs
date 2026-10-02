use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    let mut parser = CodexStreamParser::default();
    let mut report = CodexRunReport::new(Some("gpt-5.1-codex"));
    let stream = include_str!("fixtures/codex-stream.jsonl");
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
    let (server_url, server) = finish_server(2);
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

    let requests = server.join().expect("join fake Tines server");
    let completed = request_body(&requests[0]);
    let failed = request_body(&requests[1]);

    assert!(requests[0].starts_with("POST /api/v1/runs/arun_completed/finish "));
    assert!(requests[1].starts_with("POST /api/v1/runs/arun_failed/finish "));
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
}
