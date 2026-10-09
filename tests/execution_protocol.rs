use std::io::Cursor;

use serde_json::Value;
use tines_runner_rs::execution_protocol::{
    EXECUTION_PROTOCOL_VERSION, ExecutionEvent, ExecutionEventKind, ExecutionEventParser,
    ExecutionPricingEvidence, ExecutionRequest, ExecutionUsage, LogStream,
    MAX_EXECUTION_EVENT_LINE_BYTES, ProtocolError, TerminalResult, TerminalStatus,
    read_execution_request, render_event_jsonl,
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
fn missing_workspace_parent_selects_the_executor_default() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    fixture["execution"]["workspace"]
        .as_object_mut()
        .unwrap()
        .remove("parent");

    let request: ExecutionRequest = serde_json::from_value(fixture).unwrap();
    request
        .validate()
        .expect("executor default is valid policy");
    assert_eq!(request.execution.workspace.parent, None);
    assert!(
        serde_json::to_value(request).unwrap()["execution"]["workspace"]
            .get("parent")
            .is_none()
    );
}

#[test]
fn missing_repository_checkout_policy_keeps_the_existing_clone_behavior() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    fixture["execution"]
        .as_object_mut()
        .unwrap()
        .remove("repository_checkout");

    let request: ExecutionRequest = serde_json::from_value(fixture).unwrap();
    assert_eq!(
        request.execution.repository_checkout,
        tines_runner_rs::config::RepositoryCheckoutPolicy::Enabled
    );
}

#[test]
fn execution_request_can_omit_the_run_key_entirely_for_environment_delivery() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    fixture["assignment"]
        .as_object_mut()
        .unwrap()
        .remove("run_key");

    let request: ExecutionRequest = serde_json::from_value(fixture).unwrap();
    request
        .validate()
        .expect("run key is optional in the request model");
    let serialized = serde_json::to_value(request).unwrap();
    assert!(serialized["assignment"].get("run_key").is_none());
    assert!(!serialized.to_string().contains("fixture-run-key"));
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
fn request_reader_accepts_one_valid_document_and_rejects_trailing_documents() {
    let fixture = include_str!("fixtures/execution-request-v1.json");
    let request = read_execution_request(Cursor::new(fixture)).expect("valid request");
    assert_eq!(request.version, EXECUTION_PROTOCOL_VERSION);

    let two_documents = format!("{fixture}\n{fixture}");
    assert_eq!(
        read_execution_request(Cursor::new(two_documents))
            .unwrap_err()
            .to_string(),
        "malformed execution request JSON"
    );
}

#[test]
fn request_reader_reports_safe_deterministic_errors() {
    let malformed = br#"{"version":1,"assignment":{"run_key":"raw-run-key-secret",not-json}}"#;
    let malformed_error = read_execution_request(Cursor::new(malformed)).unwrap_err();
    assert_eq!(
        malformed_error.to_string(),
        "malformed execution request JSON"
    );
    assert!(!malformed_error.to_string().contains("raw-run-key-secret"));

    let mut unsupported: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    unsupported["version"] = Value::from(37);
    let unsupported_error =
        read_execution_request(Cursor::new(unsupported.to_string())).unwrap_err();
    assert_eq!(
        unsupported_error.to_string(),
        "unsupported execution protocol version 37"
    );
    assert!(!unsupported_error.to_string().contains("fixture-run-key"));

    let mut invalid: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json")).unwrap();
    invalid["assignment"]["run_key"] = Value::String("raw-run-key-secret".to_owned());
    invalid["assignment"]["timeout_minutes"] = Value::String("raw-schema-value".to_owned());
    invalid["assignment"]["env"][0]["value"] = Value::String("raw-env-secret".to_owned());
    let invalid_error = read_execution_request(Cursor::new(invalid.to_string())).unwrap_err();
    assert_eq!(invalid_error.to_string(), "invalid executor request");
    assert!(!invalid_error.to_string().contains("raw-run-key-secret"));
    assert!(!invalid_error.to_string().contains("raw-schema-value"));
    assert!(!invalid_error.to_string().contains("raw-env-secret"));
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
    assert_eq!(events.len(), 6);
    assert!(matches!(events[0].kind, ExecutionEventKind::Log { .. }));
    assert!(matches!(events[1].kind, ExecutionEventKind::Session { .. }));
    assert!(matches!(
        events[2].kind,
        ExecutionEventKind::ProviderError { .. }
    ));
    assert!(matches!(events[3].kind, ExecutionEventKind::Usage { .. }));
    assert!(matches!(
        events[4].kind,
        ExecutionEventKind::RateLimit { .. }
    ));
    assert!(matches!(events[5].kind, ExecutionEventKind::Result { .. }));
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
fn rendered_event_at_the_line_limit_is_accepted_by_the_parser() {
    let (request, _) = request_fixture();
    let empty = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: String::new(),
    });
    let empty_line = render_event_jsonl(&empty, &request).unwrap();
    let message_len = MAX_EXECUTION_EVENT_LINE_BYTES - (empty_line.len() - 1);
    let event = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: "x".repeat(message_len),
    });

    let line = render_event_jsonl(&event, &request).expect("exact-limit event renders");
    assert_eq!(line.len() - 1, MAX_EXECUTION_EVENT_LINE_BYTES);

    let mut parser = ExecutionEventParser::default();
    let parsed = parser
        .push(line.as_bytes())
        .expect("exact-limit line parses");
    assert_eq!(parsed.len(), 1);
    parser
        .push(b"{\"version\":1,\"type\":\"result\",\"status\":\"completed\",\"exit_code\":0}\n")
        .unwrap();
    parser.finish().unwrap();
}

#[test]
fn rendered_events_over_the_line_limit_are_rejected_after_json_escaping() {
    let (request, _) = request_fixture();
    let empty = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: String::new(),
    });
    let empty_line = render_event_jsonl(&empty, &request).unwrap();
    let allowed_message_len = MAX_EXECUTION_EVENT_LINE_BYTES - (empty_line.len() - 1);
    let one_byte_over = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: "x".repeat(allowed_message_len + 1),
    });
    let expected_error = ProtocolError::OversizedLine {
        line: 1,
        max_bytes: MAX_EXECUTION_EVENT_LINE_BYTES,
    };
    assert_eq!(
        render_event_jsonl(&one_byte_over, &request).unwrap_err(),
        expected_error
    );

    let escaped_message = "\n".repeat(MAX_EXECUTION_EVENT_LINE_BYTES / 2);
    let event = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: escaped_message,
    });

    assert_eq!(
        render_event_jsonl(&event, &request).unwrap_err(),
        expected_error
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
    request.execution.workspace.parent = Some("/workspace/fixture-secret".into());
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
            observed_model: None,
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
fn system_and_stderr_events_are_bounded_after_redaction_while_stdout_keeps_model_text() {
    let (request, _) = request_fixture();
    let system = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::System,
        message: format!("{}fixture-run-key{}", "x".repeat(470), "y".repeat(100)),
    });
    let rendered_system = render_event_jsonl(&system, &request).unwrap();
    let rendered_system: Value = serde_json::from_str(rendered_system.trim_end()).unwrap();
    let system_message = rendered_system["message"].as_str().unwrap();
    assert!(system_message.chars().count() <= 500);
    assert!(system_message.contains("[truncated "));
    assert!(system_message.contains("***"));
    assert!(!system_message.contains("fixture-run-key"));

    let stderr = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stderr,
        message: format!("{}fatal: remote rejected the request", "x".repeat(900)),
    });
    let rendered_stderr = render_event_jsonl(&stderr, &request).unwrap();
    let rendered_stderr: Value = serde_json::from_str(rendered_stderr.trim_end()).unwrap();
    let stderr_message = rendered_stderr["message"].as_str().unwrap();
    assert!(stderr_message.chars().count() <= 500);
    assert!(stderr_message.starts_with("… [truncated "));
    assert!(stderr_message.ends_with("fatal: remote rejected the request"));

    let model_text = "useful model text ".repeat(80);
    let stdout = ExecutionEvent::new(ExecutionEventKind::Log {
        stream: LogStream::Stdout,
        message: model_text.clone(),
    });
    let rendered_stdout = render_event_jsonl(&stdout, &request).unwrap();
    let rendered_stdout: Value = serde_json::from_str(rendered_stdout.trim_end()).unwrap();
    assert_eq!(rendered_stdout["message"], model_text);
}

#[test]
fn request_debug_redacts_secrets_before_rust_escapes_them() {
    let (mut request, _) = request_fixture();
    for secret in ["private\nvalue", "private\"value", "private\\value"] {
        request.assignment.run_key = Some(secret.to_owned());
        request.assignment.env[0].value = secret.into();
        request.execution.workspace.parent = Some(format!("/workspace/{secret}").into());

        let debug = format!("{request:?}");
        let rust_escaped = format!("{secret:?}");
        let rust_escaped = &rust_escaped[1..rust_escaped.len() - 1];
        assert!(!debug.contains(secret), "raw secret leaked for {secret:?}");
        assert!(
            !debug.contains(rust_escaped),
            "escaped secret leaked for {secret:?}"
        );
        assert!(debug.contains("[REDACTED]"));
    }
}

#[test]
fn rendered_events_redact_rust_escaped_secret_values() {
    let (mut request, _) = request_fixture();
    for secret in ["private\nvalue", "private\"value"] {
        request.assignment.run_key = Some(secret.to_owned());
        request.assignment.env[0].value = secret.into();
        let rust_escaped = format!("{secret:?}");
        let rust_escaped = &rust_escaped[1..rust_escaped.len() - 1];
        let event = ExecutionEvent::new(ExecutionEventKind::Result {
            result: TerminalResult {
                status: TerminalStatus::Failed,
                exit_code: Some(1),
                error: Some(format!("git clone failed for directory {rust_escaped}")),
                provider_session_id: None,
                observed_model: None,
                usage: None,
                pricing_evidence: None,
                interrupted: false,
                rate_limit: None,
            },
        });

        let rendered = render_event_jsonl(&event, &request).unwrap();
        assert!(
            !rendered.contains(secret),
            "raw secret leaked for {secret:?}"
        );
        assert!(
            !rendered.contains(rust_escaped),
            "escaped secret leaked for {secret:?}"
        );
        assert!(rendered.contains("***"));
    }
}

#[test]
fn terminal_result_invariants_reject_success_with_failure_details() {
    let result = TerminalResult {
        status: TerminalStatus::Completed,
        exit_code: Some(0),
        error: Some("unexpected error".into()),
        provider_session_id: None,
        observed_model: None,
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
            observed_model: None,
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
