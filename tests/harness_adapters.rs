use std::fs;
use std::path::PathBuf;

use tines_runner_rs::effort::{EffortCapabilities, EffortModelCapability};
use tines_runner_rs::execution_protocol::{
    ExecutionEvent, ExecutionEventKind, ExecutionEventParser, ExecutionRequest, TerminalStatus,
    render_event_jsonl,
};
use tines_runner_rs::executor::harness::{HarnessExit, adapter_for};
use tines_runner_rs::executor::workspace::MaterializedWorkspace;
use tines_runner_rs::executor_events::ExecutorEventStream;
use url::Url;
use uuid::Uuid;

fn request() -> ExecutionRequest {
    serde_json::from_str(include_str!("fixtures/execution-request-v1.json"))
        .expect("deserialize execution request")
}

fn workspace_for(request: &ExecutionRequest) -> (PathBuf, MaterializedWorkspace) {
    let parent = std::env::temp_dir().join(format!("tines-harness-{}", Uuid::new_v4()));
    let workspace = MaterializedWorkspace::create(
        &parent,
        &request.assignment,
        &Url::parse(&request.tines.api_url).expect("valid Tines URL"),
    )
    .expect("materialize executor workspace");
    (parent, workspace)
}

fn fixture(name: &str) -> &'static str {
    match name {
        "codex-stream.jsonl" => include_str!("fixtures/codex-stream.jsonl"),
        "codex-provider-errors.jsonl" => include_str!("fixtures/codex-provider-errors.jsonl"),
        "codex-usage-limit.jsonl" => include_str!("fixtures/codex-usage-limit.jsonl"),
        _ => unreachable!("known Codex fixture"),
    }
}

fn stderr_log_text(events: &[ExecutionEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            ExecutionEventKind::Log {
                stream: tines_runner_rs::execution_protocol::LogStream::Stderr,
                message,
            } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn semantic_selection_builds_codex_command_in_executor_path_with_safe_diagnostics() {
    let mut request = request();
    let digest = "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a";
    request.assignment.run.model = Some("gpt-5.6".to_owned());
    request
        .assignment
        .effort
        .as_mut()
        .unwrap()
        .capability_digest = Some(digest.to_owned());
    request.assignment.prompt = "Use this value: fixture-secret".to_owned();
    let (_parent, workspace) = workspace_for(&request);
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");
    assert_eq!(adapter.identifier(), "codex");
    let capabilities = EffortCapabilities {
        version: 1,
        daemon_version: "0.1.0".to_owned(),
        harness: "codex".to_owned(),
        harness_version: "codex-cli fixture".to_owned(),
        catalog_digest: digest.to_owned(),
        models: vec![EffortModelCapability {
            model: "gpt-5.6".to_owned(),
            efforts: vec!["low".to_owned(), "high".to_owned()],
        }],
        accepts_asserted_effort: None,
        discovery_error: None,
    };

    let mut launch = adapter
        .launch(&request, &workspace, &capabilities)
        .expect("build Codex launch");
    assert_eq!(launch.command().get_program(), "codex");
    let args = launch
        .command()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        &args[..5],
        [
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--model",
            "gpt-5.6"
        ]
    );
    assert_eq!(args[5], "-c");
    assert_eq!(args[6], "model_reasoning_effort=\"high\"");
    assert!(args.last().unwrap().contains("fixture-secret"));
    assert!(!args.iter().any(|arg| arg.contains("fixture-run-key")));

    let diagnostic = launch.diagnostics();
    assert!(!diagnostic.contains("fixture-run-key"));
    assert!(!diagnostic.contains("fixture-secret"));
    assert!(diagnostic.contains(&format!(
        "<prompt: {} chars>",
        request.assignment.prompt.chars().count()
    )));

    workspace.cleanup().expect("remove executor workspace");
    fs::remove_dir_all(_parent).expect("remove workspace parent");
}

#[test]
fn codex_adapter_elides_prompts_with_debug_escaped_secrets_from_launch_diagnostics() {
    let secret = "review-secret\u{1b}-suffix";
    let mut request = request();
    let digest = "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a";
    request.assignment.run.model = Some("gpt-5.6".to_owned());
    request
        .assignment
        .effort
        .as_mut()
        .unwrap()
        .capability_digest = Some(digest.to_owned());
    request.assignment.env[0].value = secret.to_owned();
    request.assignment.prompt = format!("Use {secret} in this prompt.");
    let (_parent, workspace) = workspace_for(&request);
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");
    let capabilities = EffortCapabilities {
        version: 1,
        daemon_version: "0.1.0".to_owned(),
        harness: "codex".to_owned(),
        harness_version: "codex-cli fixture".to_owned(),
        catalog_digest: digest.to_owned(),
        models: vec![EffortModelCapability {
            model: "gpt-5.6".to_owned(),
            efforts: vec!["low".to_owned(), "high".to_owned()],
        }],
        accepts_asserted_effort: None,
        discovery_error: None,
    };

    let launch = adapter
        .launch(&request, &workspace, &capabilities)
        .expect("build Codex launch");
    let diagnostic = launch.diagnostics();
    let launch_debug = format!("{launch:?}");
    let rust_escaped_secret = format!("{secret:?}").trim_matches('"').to_owned();

    for output in [diagnostic, launch_debug.as_str()] {
        assert!(!output.contains(secret), "raw secret leaked: {output}");
        assert!(
            !output.contains(&rust_escaped_secret),
            "Rust Debug-escaped secret leaked: {output}"
        );
        assert!(output.contains(&format!(
            "<prompt: {} chars>",
            request.assignment.prompt.chars().count()
        )));
    }

    workspace.cleanup().expect("remove executor workspace");
    fs::remove_dir_all(_parent).expect("remove workspace parent");
}

#[test]
fn codex_fixture_events_become_generic_protocol_events_without_native_jsonl() {
    let request = request();
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");
    let mut parser = adapter.event_parser();
    let input = fixture("codex-stream.jsonl");
    let mut events = Vec::new();
    for chunk in input.as_bytes().chunks(17) {
        events.extend(parser.push(std::str::from_utf8(chunk).expect("ASCII fixture chunk")));
    }
    events.extend(parser.finish());

    assert!(events.iter().any(|event| matches!(
        event.kind,
        ExecutionEventKind::Session { ref provider, ref id }
            if provider == "codex" && id == "01a09e68-24d8-78d3-8bc7-67037a0cd7de"
    )));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        ExecutionEventKind::Usage { ref usage }
            if usage.input_tokens == Some(8_913)
                && usage.output_tokens == Some(1_980)
                && usage.cache_read_tokens == Some(101_888)
                && usage.cache_write_tokens == Some(0)
    )));

    let rendered = events
        .iter()
        .map(|event| render_event_jsonl(event, &request).expect("render generic event"))
        .collect::<String>();
    assert!(!rendered.contains("thread.started"));
    assert!(!rendered.contains("turn.completed"));
    assert!(!rendered.contains("thread.metadata.updated"));

    let mut protocol_parser = ExecutionEventParser::default();
    let decoded = protocol_parser
        .push(rendered.as_bytes())
        .expect("generic events use protocol v1");
    assert_eq!(decoded, events);
}

#[test]
fn codex_stderr_redacts_run_keys_split_across_reads_in_raw_and_escaped_forms() {
    let secret = r#"review"fixture\environment-key"#;
    let escaped_secret = r#"review\"fixture\\environment-key"#;
    let mut request = request();
    request.assignment.run_key = Some(secret.to_owned());
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");

    for chunks in [
        vec![
            r#"raw prefix review""#,
            r#"fixture\env"#,
            "ironment-key raw suffix",
        ],
        vec![
            r#"escaped prefix review\"#,
            r#""fixture\\envi"#,
            "ronment-key escaped suffix",
        ],
    ] {
        let mut parser = adapter.event_parser_for_request(&request);
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.push_stderr(chunk));
        }
        events.extend(parser.finish_stderr());
        let rendered = events
            .iter()
            .map(|event| render_event_jsonl(event, &request).expect("render stderr event"))
            .collect::<String>();
        let mut protocol_parser = ExecutionEventParser::default();
        let decoded = protocol_parser
            .push(rendered.as_bytes())
            .expect("stderr events use protocol v1");
        let text = stderr_log_text(&decoded);

        assert!(text.contains("***"), "run key was not redacted: {text:?}");
        assert!(!text.contains(secret), "raw run key leaked: {text:?}");
        assert!(
            !text.contains(escaped_secret),
            "escaped run key leaked: {text:?}"
        );
    }
}

#[test]
fn provider_errors_rate_limits_and_terminal_result_are_normalized() {
    let request = request();
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");
    let mut parser = adapter.event_parser();
    let mut events = parser.push(fixture("codex-provider-errors.jsonl"));
    events.extend(parser.finish());

    assert!(events.iter().any(|event| matches!(
        event.kind,
        ExecutionEventKind::ProviderError { ref provider, ref code, .. }
            if provider == "codex" && code.as_deref() == Some("unauthorized")
    )));
    let mut limit_parser = adapter.event_parser();
    let mut limit_events = limit_parser.push(fixture("codex-usage-limit.jsonl"));
    limit_events.extend(limit_parser.finish());
    assert!(
        limit_events
            .iter()
            .any(|event| matches!(event.kind, ExecutionEventKind::RateLimit { .. }))
    );

    let terminal = limit_parser.terminal_result(HarnessExit {
        exit_code: Some(1),
        error: None,
        interrupted: false,
    });
    assert!(matches!(
        terminal.kind,
        ExecutionEventKind::Result { ref result }
            if result.status == TerminalStatus::RateLimited
                && result.exit_code == Some(1)
                && result.rate_limit.is_some()
                && result.provider_session_id.as_deref() == Some("thread-limited")
                && result.usage.as_ref().is_some_and(|usage|
                    usage.input_tokens == Some(95)
                        && usage.output_tokens == Some(8)
                        && usage.cache_read_tokens == Some(20)
                        && usage.cache_write_tokens == Some(5))
    ));
    terminal
        .validate()
        .expect("valid normalized terminal result");

    let mut zero_exit_parser = adapter.event_parser();
    zero_exit_parser.push(fixture("codex-usage-limit.jsonl"));
    zero_exit_parser.finish();
    let zero_exit_result = zero_exit_parser.terminal_result(HarnessExit {
        exit_code: Some(0),
        error: None,
        interrupted: false,
    });
    assert!(matches!(
        zero_exit_result.kind,
        ExecutionEventKind::Result { ref result }
            if result.status == TerminalStatus::RateLimited && result.exit_code.is_none()
    ));
    zero_exit_result
        .validate()
        .expect("rate limit is terminal even when a harness exits zero");
}

#[test]
fn codex_usage_and_pricing_evidence_survive_the_split_executor_finish_path() {
    let mut request = request();
    request.assignment.run.model = Some("gpt-5.1-codex".to_owned());
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");

    for (tail, exit, expected_status, expected_finish) in [
        (
            "",
            HarnessExit {
                exit_code: Some(0),
                error: None,
                interrupted: false,
            },
            "complete",
            tines_runner_rs::protocol::FinishStatus::Completed,
        ),
        (
            "{\"type\":\"turn.started\"}\n",
            HarnessExit {
                exit_code: Some(7),
                error: Some("harness failed after usage".to_owned()),
                interrupted: false,
            },
            "incomplete_attempt",
            tines_runner_rs::protocol::FinishStatus::Failed,
        ),
    ] {
        let codex = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"01a09e68-24d8-78d3-8bc7-67037a0cd7de\"}}\n{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":30,\"cached_input_tokens\":5,\"cache_write_input_tokens\":2,\"output_tokens\":7}}}}\n{tail}"
        );
        let mut codex_parser = adapter.event_parser_for_request(&request);
        let mut events = Vec::new();
        for chunk in codex.as_bytes().chunks(19) {
            events.extend(codex_parser.push(std::str::from_utf8(chunk).expect("ASCII fixture")));
        }
        events.extend(codex_parser.finish());
        let terminal = codex_parser.terminal_result(exit);
        events.push(terminal.clone());

        let terminal_value = serde_json::to_value(&terminal).expect("serialize terminal event");
        assert_eq!(terminal_value["type"], "result");
        assert_eq!(terminal_value["usage"]["input_tokens"], 23);
        assert_eq!(terminal_value["pricing_evidence"]["provider"], "codex");
        assert_eq!(terminal_value["pricing_evidence"]["version"], 1);
        let payload = &terminal_value["pricing_evidence"]["payload"];
        assert_eq!(payload["version"], 1);
        assert_eq!(payload["harness"], "codex");
        assert_eq!(payload["model"], "gpt-5.1-codex");
        assert_eq!(payload["identity_source"], "launch_argument");
        assert_eq!(payload["usage_scope"], "thread_total");
        assert_eq!(payload["session_mode"], "cold");
        assert_eq!(payload["normalization"], "codex-jsonl-v1");
        assert_eq!(payload["raw_usage"]["input_tokens"], 30);
        assert_eq!(payload["raw_usage"]["cached_input_tokens"], 5);
        assert_eq!(payload["raw_usage"]["cache_write_input_tokens"], 2);
        assert_eq!(payload["raw_usage"]["output_tokens"], 7);
        assert_eq!(payload["measurement_status"], expected_status);
        assert_eq!(payload["terminal_snapshots"], 1);

        let mut daemon_stream = ExecutorEventStream::new(&request);
        for event in events {
            let jsonl = render_event_jsonl(&event, &request).expect("render executor event");
            daemon_stream
                .push(jsonl.as_bytes())
                .expect("daemon accepts executor event");
        }
        daemon_stream.finish().expect("daemon sees terminal result");
        let finish = daemon_stream.finish_request(None, "", false);
        assert_eq!(finish.status, expected_finish);
        assert_eq!(finish.usage.as_ref().unwrap().input_tokens, Some(23));
        assert_eq!(finish.usage.as_ref().unwrap().cache_read_tokens, Some(5));
        let evidence = finish
            .pricing_evidence
            .expect("pricing evidence reaches Tines");
        assert_eq!(evidence.model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(evidence.raw_usage.as_ref().unwrap().input_tokens, Some(30));
        assert_eq!(
            serde_json::to_value(evidence).unwrap()["measurement_status"],
            expected_status
        );
    }
}

#[test]
fn codex_evidence_marks_missing_and_invalid_terminal_usage_without_fabricating_counts() {
    let mut request = request();
    request.assignment.run.model = Some("gpt-5.1-codex".to_owned());
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");

    for (input, expected_status, expected_snapshots) in [
        ("", "missing", 0),
        ("{\"type\":\"turn.completed\"}\n", "missing", 1),
        (
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":3,\"cached_input_tokens\":4,\"cache_write_input_tokens\":0,\"output_tokens\":2}}\n",
            "invalid",
            1,
        ),
    ] {
        let mut parser = adapter.event_parser_for_request(&request);
        parser.push(input);
        parser.finish();
        let terminal = parser.terminal_result(HarnessExit {
            exit_code: Some(0),
            error: None,
            interrupted: false,
        });
        let value = serde_json::to_value(terminal).expect("serialize terminal result");
        let evidence = &value["pricing_evidence"]["payload"];
        assert_eq!(evidence["measurement_status"], expected_status);
        assert_eq!(evidence["terminal_snapshots"], expected_snapshots);
        if expected_status == "invalid" {
            assert_eq!(evidence["raw_usage"]["input_tokens"], 3);
            assert_eq!(evidence["raw_usage"]["cached_input_tokens"], 4);
            assert!(value["usage"].get("input_tokens").is_none());
        } else if expected_snapshots == 1 {
            assert_eq!(evidence["raw_usage"], serde_json::json!({}));
            assert!(value.get("usage").is_none());
        } else {
            assert!(evidence.get("raw_usage").is_none());
            assert!(value.get("usage").is_none());
        }
    }
}

#[test]
fn codex_error_events_redact_run_key_and_secret_environment_values() {
    let request = request();
    let adapter = adapter_for(&request.execution.harness).expect("select Codex adapter");
    let mut parser = adapter.event_parser();
    let events = parser.push(
        r#"{"type":"error","message":"fixture-run-key fixture-secret"}
"#,
    );
    let rendered = events
        .iter()
        .map(|event| render_event_jsonl(event, &request).expect("render safe event"))
        .collect::<String>();

    assert!(!rendered.contains("fixture-run-key"));
    assert!(!rendered.contains("fixture-secret"));
    assert!(!rendered.contains(r#""type":"error""#));
    assert!(rendered.contains("***"));
}

#[test]
fn unknown_harnesses_fail_selection_without_echoing_request_values() {
    let error = match adapter_for("fixture-secret") {
        Ok(_) => panic!("unknown harness was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "unsupported execution harness");
}
