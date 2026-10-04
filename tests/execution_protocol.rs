use serde_json::Value;
use tines_runner_rs::execution_protocol::{
    EXECUTION_PROTOCOL_VERSION, ExecutionEvent, ExecutionEventKind, ExecutionEventParser,
    ExecutionPricingEvidence, ExecutionRequest, ExecutionUsage, LogStream,
    MAX_EXECUTION_EVENT_LINE_BYTES, ProtocolError, TerminalResult, TerminalStatus,
    render_event_jsonl,
};

fn request_fixture() -> (ExecutionRequest, Value) {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/execution-request-v1.json"))
        .expect("valid request fixture");
    let request: ExecutionRequest =
        serde_json::from_value(fixture.clone()).expect("deserialize execution request");
    (request, fixture)
}

#[test]
fn execution_request_fixture_round_trips_the_assignment_and_local_policy() {
    let (request, fixture) = request_fixture();
    request.validate().expect("valid request policy and URL");
    assert_eq!(serde_json::to_value(request).unwrap(), fixture);
}

#[test]
fn unsupported_request_versions_are_rejected_during_deserialization() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    fixture["version"] = Value::from(EXECUTION_PROTOCOL_VERSION + 1);

    let error = serde_json::from_value::<ExecutionRequest>(fixture).unwrap_err();
    assert_eq!(
        error.to_string(),
        "unsupported execution protocol version 2"
    );
}

#[test]
fn event_fixture_deserializes_and_serializes_every_known_event_variant() {
    let fixture = include_str!("fixtures/execution-events-v1.jsonl");
    let expected = fixture
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let mut parser = ExecutionEventParser::default();
    let mut events = Vec::new();
    for chunk in fixture.as_bytes().chunks(19) {
        events.extend(parser.push(chunk).expect("parse event chunk"));
    }
    assert_eq!(events.len(), 5);
    assert!(matches!(events[0].kind, ExecutionEventKind::Log { .. }));
    assert!(matches!(events[1].kind, ExecutionEventKind::Session { .. }));
    assert!(matches!(events[2].kind, ExecutionEventKind::Usage { .. }));
    assert!(matches!(
        events[3].kind,
        ExecutionEventKind::RateLimit { .. }
    ));
    assert!(matches!(events[4].kind, ExecutionEventKind::Result { .. }));
    assert!(parser.finish().unwrap().is_none());

    let (request, _) = request_fixture();
    let rendered = events
        .iter()
        .map(|event| {
            let line = render_event_jsonl(event, &request).expect("render event");
            serde_json::from_str::<Value>(line.trim_end()).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(rendered, expected);
}

#[test]
fn unknown_additive_events_are_ignored_but_a_terminal_result_is_required() {
    let mut parser = ExecutionEventParser::default();
    let events = parser
        .push(
            br#"{"version":1,"type":"future_metric","count":7,"metadata":{"source":"new executor"}}
{"version":1,"type":"result","status":"completed","exit_code":0}
"#,
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].kind, ExecutionEventKind::Result { .. }));
    parser.finish().unwrap();
}

#[test]
fn unknown_fields_on_known_events_are_ignored() {
    let mut parser = ExecutionEventParser::default();
    let events = parser
        .push(
            br#"{"version":1,"type":"log","stream":"stdout","message":"working","future_field":{"enabled":true}}
{"version":1,"type":"result","status":"completed","exit_code":0}
"#,
        )
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0].kind, ExecutionEventKind::Log { .. }));
    parser.finish().unwrap();
}

#[test]
fn malformed_jsonl_is_rejected_without_echoing_the_line() {
    let mut parser = ExecutionEventParser::default();
    let error = parser.push(b"{\"version\":1,not-json}\n").unwrap_err();
    assert_eq!(error, ProtocolError::MalformedJson { line: 1 });
    assert!(!error.to_string().contains("not-json"));
}

#[test]
fn oversized_jsonl_lines_are_rejected_at_the_fixed_bound() {
    let mut parser = ExecutionEventParser::default();
    let line = vec![b'x'; MAX_EXECUTION_EVENT_LINE_BYTES + 1];
    assert_eq!(
        parser.push(&line).unwrap_err(),
        ProtocolError::OversizedLine {
            line: 1,
            max_bytes: MAX_EXECUTION_EVENT_LINE_BYTES,
        }
    );
}

#[test]
fn unsupported_event_versions_are_rejected_deterministically() {
    let mut parser = ExecutionEventParser::default();
    assert_eq!(
        parser
            .push(b"{\"version\":2,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}\n")
            .unwrap_err(),
        ProtocolError::UnsupportedVersionValue {
            line: 1,
            version: 2,
        }
    );
}

#[test]
fn eof_without_a_terminal_result_is_an_error() {
    let mut parser = ExecutionEventParser::default();
    parser
        .push(b"{\"version\":1,\"type\":\"log\",\"stream\":\"stdout\",\"message\":\"working\"}\n")
        .unwrap();
    assert_eq!(
        parser.finish().unwrap_err(),
        ProtocolError::MissingTerminalResult
    );
}

#[test]
fn final_unterminated_result_is_accepted() {
    let mut parser = ExecutionEventParser::default();
    assert!(
        parser
            .push(b"{\"version\":1,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}")
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        parser.finish().unwrap().unwrap().kind,
        ExecutionEventKind::Result { .. }
    ));
}

#[test]
fn duplicate_terminal_results_are_rejected() {
    let result = b"{\"version\":1,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}\n";
    let mut parser = ExecutionEventParser::default();
    parser.push(result).unwrap();
    assert_eq!(
        parser.push(result).unwrap_err(),
        ProtocolError::DuplicateTerminalResult { line: 2 }
    );
}

#[test]
fn any_other_output_after_a_terminal_result_is_rejected() {
    let mut parser = ExecutionEventParser::default();
    parser
        .push(b"{\"version\":1,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}\n")
        .unwrap();
    assert_eq!(
        parser
            .push(b"{\"version\":1,\"type\":\"log\",\"stream\":\"stdout\",\"message\":\"late\"}\n")
            .unwrap_err(),
        ProtocolError::OutputAfterTerminal { line: 2 }
    );
}

#[test]
fn request_debug_and_rendered_events_redact_the_run_key_and_secret_environment_values() {
    let (mut request, _) = request_fixture();
    request.execution.harness = "fixture-run-key".into();
    request.execution.workspace.parent = "/workspace/fixture-secret".into();
    let debug = format!("{request:?}");
    assert!(!debug.contains("fixture-run-key"));
    assert!(!debug.contains("fixture-secret"));
    assert!(!debug.contains("Implement the assigned issue"));

    let event = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: "keys fixture-run-key and fixture-secret".into(),
    });
    let rendered = render_event_jsonl(&event, &request).unwrap();
    assert!(!rendered.contains("fixture-run-key"));
    assert!(!rendered.contains("fixture-secret"));
    assert!(rendered.contains("***"));
    assert!(!format!("{event:?}").contains("fixture-run-key"));

    let result = ExecutionEvent::new(ExecutionEventKind::Result {
        result: TerminalResult {
            status: TerminalStatus::Failed,
            exit_code: Some(1),
            error: Some("fixture-run-key".into()),
            provider_session_id: None,
            usage: None,
            pricing_evidence: Some(ExecutionPricingEvidence {
                provider: "example-provider".into(),
                version: 1,
                payload: serde_json::json!({
                    "fixture-run-key": "fixture-secret"
                }),
            }),
            interrupted: false,
            rate_limit: None,
        },
    });
    let rendered = render_event_jsonl(&result, &request).unwrap();
    assert!(!rendered.contains("fixture-run-key"));
    assert!(!rendered.contains("fixture-secret"));
}

#[test]
fn terminal_result_invariants_reject_success_with_failure_details() {
    let result = TerminalResult {
        status: TerminalStatus::Completed,
        exit_code: Some(0),
        error: Some("unexpected error".into()),
        provider_session_id: None,
        usage: Some(ExecutionUsage {
            output_tokens: Some(4),
            ..ExecutionUsage::default()
        }),
        pricing_evidence: None,
        interrupted: false,
        rate_limit: None,
    };
    assert_eq!(result.validate(), Err(ProtocolError::InvalidTerminalResult));
}

#[test]
fn interrupted_terminal_result_round_trips() {
    let (request, _) = request_fixture();
    let event = ExecutionEvent::new(ExecutionEventKind::Result {
        result: TerminalResult {
            status: TerminalStatus::Failed,
            exit_code: None,
            error: None,
            provider_session_id: Some("session-interrupted".into()),
            usage: None,
            pricing_evidence: None,
            interrupted: true,
            rate_limit: None,
        },
    });
    let line = render_event_jsonl(&event, &request).unwrap();
    let mut parser = ExecutionEventParser::default();
    let parsed = parser.push(line.as_bytes()).unwrap();
    assert!(matches!(
        &parsed[0].kind,
        ExecutionEventKind::Result { result } if result.interrupted
    ));
    parser.finish().unwrap();
}
