use std::fs;
use std::path::PathBuf;

use tines_runner_rs::effort::{EffortCapabilities, EffortModelCapability};
use tines_runner_rs::execution_protocol::{
    ExecutionEventKind, ExecutionEventParser, ExecutionRequest, TerminalStatus, render_event_jsonl,
};
use tines_runner_rs::harness::{HarnessExit, adapter_for};
use tines_runner_rs::workspace::MaterializedWorkspace;
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
    assert!(diagnostic.contains("[REDACTED]"));

    workspace.cleanup().expect("remove executor workspace");
    fs::remove_dir_all(_parent).expect("remove workspace parent");
}

#[test]
fn codex_adapter_redacts_debug_escaped_secrets_before_formatting_launch_diagnostics() {
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
        assert!(output.contains("[REDACTED]"));
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
