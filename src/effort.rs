//! Codex effort capability discovery and assignment verification.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protocol::{RunnerAssignment, RunnerAssignmentEffort};

const DISCOVERY_DEADLINE: Duration = Duration::from_secs(5);
const MAX_STDOUT: usize = 1024 * 1024;
pub const MAX_EFFORT_CAPABILITIES_AGE: Duration = Duration::from_secs(10 * 60);
const MAX_MODELS: usize = 256;
const MAX_EFFORTS_PER_MODEL: usize = 16;
const RECOGNIZED_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EffortCapabilities {
    pub version: u8,
    pub daemon_version: String,
    pub harness: String,
    pub harness_version: String,
    pub catalog_digest: String,
    pub models: Vec<EffortModelCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepts_asserted_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EffortModelCapability {
    pub model: String,
    pub efforts: Vec<String>,
}

impl EffortCapabilities {
    /// Discover the Codex version and the exact model/effort catalog it exposes.
    /// Discovery failures are reported as an empty, non-asserting catalog.
    pub fn discover(daemon_version: &str) -> Self {
        Self::discover_with_program("codex", daemon_version)
    }

    /// Discover Codex capabilities through the legacy launcher's argv prefix.
    pub fn discover_with_wrapper(wrapper: &[String], daemon_version: &str) -> Self {
        let Some((program, arguments)) = wrapper.split_first() else {
            return Self::discover(daemon_version);
        };
        let mut prefix = arguments.to_vec();
        prefix.push("codex".to_owned());
        Self::discover_with_prefix(program, &prefix, daemon_version)
    }

    fn discover_with_program(program: &str, daemon_version: &str) -> Self {
        Self::discover_with_prefix(program, &[], daemon_version)
    }

    fn discover_with_prefix(program: &str, prefix: &[String], daemon_version: &str) -> Self {
        let harness_version = match run_capture(program, prefix, &["--version"]) {
            Ok(output) => match String::from_utf8(output) {
                Ok(version) if !version.trim().is_empty() => truncate(version.trim(), 100),
                Ok(_) => return Self::failure(daemon_version, "Codex returned an empty version"),
                Err(_) => return Self::failure(daemon_version, "Codex version was not UTF-8"),
            },
            Err(error) => return Self::failure(daemon_version, &error),
        };

        match discover_models(program, prefix, daemon_version, harness_version.clone()) {
            Ok(capabilities) => capabilities,
            Err(error) => Self::catalog_failure(daemon_version, &harness_version, &error),
        }
    }

    fn failure(daemon_version: &str, reason: &str) -> Self {
        Self::unavailable(daemon_version, "codex", reason)
    }

    /// Build a valid empty report when an executor cannot verify a harness.
    pub fn unavailable(daemon_version: &str, harness: &str, reason: &str) -> Self {
        Self::unavailable_with_version(daemon_version, harness, "unknown", reason)
    }

    fn catalog_failure(daemon_version: &str, harness_version: &str, reason: &str) -> Self {
        Self::unavailable_with_version(daemon_version, "codex", harness_version, reason)
    }

    fn unavailable_with_version(
        daemon_version: &str,
        harness: &str,
        harness_version: &str,
        reason: &str,
    ) -> Self {
        let models = Vec::new();
        Self {
            version: 1,
            daemon_version: truncate(daemon_version, 100),
            harness: truncate(harness, 100),
            harness_version: truncate(harness_version, 100),
            catalog_digest: catalog_digest(&models),
            models,
            accepts_asserted_effort: None,
            discovery_error: Some(truncate(reason, 200)),
        }
    }

    /// Return true after the report has expired or when no report exists yet.
    pub fn refresh_due(&self, refreshed_at: Option<Instant>, now: Instant) -> bool {
        refreshed_at
            .is_none_or(|at| now.saturating_duration_since(at) >= MAX_EFFORT_CAPABILITIES_AGE)
    }

    /// Check that this report has a valid catalog and belongs to `harness`.
    pub fn validate_for_harness(&self, harness: &str) -> Result<(), String> {
        if self.version != 1 || self.harness != harness {
            return Err("unsupported effort capability report".to_owned());
        }
        if self.harness_version.trim().is_empty()
            || self.harness_version.len() > 100
            || self.daemon_version.len() > 100
            || self.catalog_digest.len() > 100
        {
            return Err("malformed effort capability report".to_owned());
        }
        validate_catalog(&self.models)?;
        if catalog_digest(&self.models) != self.catalog_digest {
            return Err("effort capability catalog digest is invalid".to_owned());
        }
        Ok(())
    }

    /// Whether this report confirms that the named harness is installed in
    /// the environment that produced the report.
    pub fn supports_harness(&self, harness: &str) -> bool {
        self.validate_for_harness(harness).is_ok() && self.harness_version != "unknown"
    }
}

/// Reject an enforced effort unless the fresh local report verifies its contract.
pub fn assignment_effort_rejection(
    assignment: &RunnerAssignment,
    capabilities: &EffortCapabilities,
) -> Option<String> {
    let effort = assignment.effort.as_ref()?;
    verify_effort(assignment.run.model.as_deref(), effort, capabilities).err()
}

fn verify_effort(
    model: Option<&str>,
    effort: &RunnerAssignmentEffort,
    capabilities: &EffortCapabilities,
) -> Result<(), String> {
    if effort.version != 1 {
        return Err(format!(
            "unsupported assignment effort version {}",
            effort.version
        ));
    }
    if capabilities.version != 1 || capabilities.harness != "codex" {
        return Err("effort assignment requires a compatible Codex capability report".to_owned());
    }
    if capabilities.discovery_error.is_some() || capabilities.harness_version == "unknown" {
        return Err("installed Codex effort support could not be verified".to_owned());
    }
    if catalog_digest(&capabilities.models) != capabilities.catalog_digest {
        return Err("local Codex effort catalog digest is invalid".to_owned());
    }
    let Some(delivered_digest) = effort.capability_digest.as_deref() else {
        return Err("effort assignment has no capability catalog digest".to_owned());
    };
    if delivered_digest != capabilities.catalog_digest {
        return Err("Codex effort capability catalog changed after assignment delivery".to_owned());
    }

    let listed = model.and_then(|model| {
        capabilities
            .models
            .iter()
            .find(|entry| entry.model == model)
    });
    if let Some(listed) = listed {
        return if listed
            .efforts
            .iter()
            .any(|supported| supported == &effort.value)
        {
            Ok(())
        } else {
            Err(format!(
                "{} does not support effort {}",
                model.unwrap_or("the assigned model"),
                effort.value
            ))
        };
    }

    let model_label = model.unwrap_or("the assigned model");
    if effort.verification.as_deref() == Some("asserted")
        && capabilities.accepts_asserted_effort == Some(true)
        && RECOGNIZED_EFFORTS.contains(&effort.value.as_str())
    {
        return Ok(());
    }
    Err(format!(
        "{model_label} effort support could not be verified"
    ))
}

fn catalog_digest(models: &[EffortModelCapability]) -> String {
    let encoded = serde_json::to_vec(models).expect("effort catalog is serializable");
    let digest = Sha256::digest(encoded);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn run_capture(program: &str, prefix: &[String], args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program)
        .args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run codex --version: {error}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || read_bounded(stdout, sender));
    let started = Instant::now();
    let status = match wait_child(&mut child, started) {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(receiver);
            return Err(error);
        }
    };
    let output = receiver
        .recv_timeout(DISCOVERY_DEADLINE.saturating_sub(started.elapsed()))
        .map_err(|_| "could not read Codex version output".to_owned())??;
    let _ = reader.join();
    if !status.success() {
        return Err(format!("codex --version exited with {status}"));
    }
    Ok(output)
}

fn read_bounded(mut reader: impl Read, sender: mpsc::Sender<Result<Vec<u8>, String>>) {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    let mut too_large = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if output.len().saturating_add(count) > MAX_STDOUT {
                    too_large = true;
                } else if !too_large {
                    output.extend_from_slice(&chunk[..count]);
                }
            }
            Err(error) => {
                let _ = sender.send(Err(format!("could not read Codex output: {error}")));
                return;
            }
        }
    }
    let result = if too_large {
        Err("Codex output exceeded 1 MiB".to_owned())
    } else {
        Ok(output)
    };
    let _ = sender.send(result);
}

fn wait_child(child: &mut Child, started: Instant) -> Result<std::process::ExitStatus, String> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not wait for Codex: {error}"))?
        {
            return Ok(status);
        }
        if started.elapsed() >= DISCOVERY_DEADLINE {
            return Err("Codex version discovery timed out".to_owned());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn discover_models(
    program: &str,
    prefix: &[String],
    daemon_version: &str,
    harness_version: String,
) -> Result<EffortCapabilities, String> {
    let mut child = Command::new(program)
        .args(prefix)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start Codex app-server: {error}"))?;
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || read_lines_bounded(stdout, sender));
    let result = query_model_catalog(&mut child, stdin, receiver, daemon_version, harness_version);
    let _ = child.kill();
    let _ = child.wait();
    drop(reader);
    result
}

enum AppServerLine {
    Line(String),
    Eof,
    Error(String),
}

fn read_lines_bounded(mut reader: impl Read, sender: mpsc::Sender<AppServerLine>) {
    let mut line = Vec::new();
    let mut total = 0usize;
    let mut chunk = [0; 8192];
    loop {
        let remaining = MAX_STDOUT.saturating_sub(total);
        let read_limit = chunk.len().min(remaining.saturating_add(1));
        match reader.read(&mut chunk[..read_limit]) {
            Ok(0) => {
                if !line.is_empty() {
                    let content = String::from_utf8_lossy(&line).trim().to_owned();
                    if sender.send(AppServerLine::Line(content)).is_err() {
                        return;
                    }
                }
                let _ = sender.send(AppServerLine::Eof);
                return;
            }
            Ok(count) => {
                if total.saturating_add(count) > MAX_STDOUT {
                    let _ = sender.send(AppServerLine::Error(
                        "Codex model discovery exceeded 1 MiB".to_owned(),
                    ));
                    return;
                }
                total += count;

                let mut start = 0;
                while let Some(offset) = chunk[start..count].iter().position(|byte| *byte == b'\n')
                {
                    let end = start + offset + 1;
                    line.extend_from_slice(&chunk[start..end]);
                    let content = String::from_utf8_lossy(&line).trim().to_owned();
                    if sender.send(AppServerLine::Line(content)).is_err() {
                        return;
                    }
                    line.clear();
                    start = end;
                }
                if start < count {
                    line.extend_from_slice(&chunk[start..count]);
                }
            }
            Err(error) => {
                let _ = sender.send(AppServerLine::Error(format!(
                    "could not read Codex app-server output: {error}"
                )));
                return;
            }
        }
    }
}

fn query_model_catalog(
    child: &mut Child,
    mut stdin: std::process::ChildStdin,
    receiver: Receiver<AppServerLine>,
    daemon_version: &str,
    harness_version: String,
) -> Result<EffortCapabilities, String> {
    write_message(
        &mut stdin,
        &serde_json::json!({
            "id": 1,
            "method": "initialize",
            "params": { "clientInfo": { "name": "tines", "version": daemon_version } }
        }),
    )?;
    let started = Instant::now();
    let mut initialized = false;
    let mut models = Vec::new();
    let mut next_request_id = 2u64;

    loop {
        let remaining = DISCOVERY_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("Codex model discovery timed out".to_owned());
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(AppServerLine::Line(line)) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message["id"] == 1 && message.get("result").is_some() {
                    if !initialized {
                        initialized = true;
                        write_message(
                            &mut stdin,
                            &serde_json::json!({ "method": "initialized", "params": {} }),
                        )?;
                        next_request_id += 1;
                        write_model_list_request(&mut stdin, next_request_id, None)?;
                    }
                } else if message["id"].as_u64().is_some_and(|id| id >= 3)
                    && message.get("result").is_some()
                {
                    let result = &message["result"];
                    let Some(data) = result.get("data").and_then(Value::as_array) else {
                        return Err("Codex model/list returned no model data".to_owned());
                    };
                    for entry in data {
                        let Some(model) = entry.get("model").and_then(Value::as_str) else {
                            continue;
                        };
                        let mut efforts = Vec::new();
                        if let Some(supported) = entry
                            .get("supportedReasoningEfforts")
                            .and_then(Value::as_array)
                        {
                            for item in supported {
                                if let Some(value) =
                                    item.get("reasoningEffort").and_then(Value::as_str)
                                    && !efforts.iter().any(|present| present == value)
                                {
                                    efforts.push(value.to_owned());
                                }
                            }
                        }
                        if !efforts.is_empty() {
                            models.push(EffortModelCapability {
                                model: model.to_owned(),
                                efforts,
                            });
                        }
                    }
                    match result.get("nextCursor") {
                        Some(Value::String(cursor)) if !cursor.is_empty() => {
                            next_request_id += 1;
                            write_model_list_request(&mut stdin, next_request_id, Some(cursor))?;
                        }
                        None | Some(Value::Null) | Some(Value::String(_)) => {
                            validate_catalog(&models)?;
                            return Ok(success_report(daemon_version, harness_version, models));
                        }
                        Some(_) => {
                            return Err("Codex model/list returned an invalid cursor".to_owned());
                        }
                    }
                }
            }
            Ok(AppServerLine::Eof) => {
                let status = child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "closed stdout".to_owned());
                return Err(format!(
                    "Codex app-server exited ({status}) before listing models"
                ));
            }
            Ok(AppServerLine::Error(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("could not wait for Codex app-server: {error}"))?
                {
                    return Err(format!(
                        "Codex app-server exited ({status}) before listing models"
                    ));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Codex app-server output closed before listing models".to_owned());
            }
        }
    }
}

fn validate_catalog(models: &[EffortModelCapability]) -> Result<(), String> {
    if models.len() > MAX_MODELS {
        return Err("Codex effort catalog exceeded 256 models".to_owned());
    }
    let mut names = HashSet::new();
    for model in models {
        if model.model.is_empty() || model.model.len() > 200 || !names.insert(&model.model) {
            return Err("Codex effort catalog contained an invalid or duplicate model".to_owned());
        }
        if model.efforts.len() > MAX_EFFORTS_PER_MODEL
            || model.efforts.iter().any(|effort| !is_effort_token(effort))
        {
            return Err("Codex effort catalog contained an invalid effort value".to_owned());
        }
    }
    Ok(())
}

fn is_effort_token(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
        })
}

fn success_report(
    daemon_version: &str,
    harness_version: String,
    models: Vec<EffortModelCapability>,
) -> EffortCapabilities {
    let catalog_digest = catalog_digest(&models);
    EffortCapabilities {
        version: 1,
        daemon_version: truncate(daemon_version, 100),
        harness: "codex".to_owned(),
        harness_version: truncate(&harness_version, 100),
        catalog_digest,
        models,
        accepts_asserted_effort: Some(true),
        discovery_error: None,
    }
}

fn write_model_list_request(
    stdin: &mut impl Write,
    id: u64,
    cursor: Option<&str>,
) -> Result<(), String> {
    let mut params = serde_json::Map::new();
    params.insert("includeHidden".to_owned(), Value::Bool(true));
    if let Some(cursor) = cursor {
        params.insert("cursor".to_owned(), Value::String(cursor.to_owned()));
    }
    write_message(
        stdin,
        &serde_json::json!({ "id": id, "method": "model/list", "params": params }),
    )
}

fn write_message(stdin: &mut impl Write, message: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *stdin, message)
        .map_err(|error| format!("could not encode Codex discovery request: {error}"))?;
    stdin
        .write_all(b"\n")
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("Codex app-server stdin: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        AppServerLine, EffortCapabilities, EffortModelCapability, MAX_STDOUT,
        assignment_effort_rejection, catalog_digest, read_lines_bounded, success_report,
        verify_effort,
    };
    use crate::protocol::{RunReference, RunnerAssignment, RunnerAssignmentEffort};
    use std::io::{self, Read};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn capabilities() -> EffortCapabilities {
        success_report(
            "0.1.0",
            "codex-cli 0.153.4".to_owned(),
            vec![EffortModelCapability {
                model: "gpt-5.6".to_owned(),
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
        )
    }

    fn assignment(
        model: Option<&str>,
        version: u8,
        value: &str,
        digest: Option<&str>,
        verification: Option<&str>,
    ) -> RunnerAssignment {
        RunnerAssignment {
            run: RunReference {
                id: "arun_test".to_owned(),
                issue_id: "iss_test".to_owned(),
                model: model.map(str::to_owned),
                issue_ref: None,
                state_at_start_name: None,
            },
            effort: Some(RunnerAssignmentEffort {
                version,
                value: value.to_owned(),
                capability_digest: digest.map(str::to_owned),
                verification: verification.map(str::to_owned),
            }),
            prompt: String::new(),
            bundle: serde_json::Value::Null,
            run_key: String::new(),
            env: Vec::new(),
            timeout_minutes: 1,
        }
    }

    #[test]
    fn catalog_digest_matches_compact_serialized_model_contract() {
        let catalog = vec![EffortModelCapability {
            model: "gpt-5.6".to_owned(),
            efforts: vec!["low".to_owned(), "high".to_owned()],
        }];
        assert_eq!(
            catalog_digest(&catalog),
            "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a"
        );
    }

    #[test]
    fn assignment_effort_requires_an_exact_digest_model_and_value() {
        let report = capabilities();
        let digest = report.catalog_digest.clone();
        assert_eq!(
            assignment_effort_rejection(
                &assignment(Some("gpt-5.6"), 1, "high", Some(&digest), None),
                &report
            ),
            None
        );
        assert!(
            verify_effort(
                Some("gpt-5.6"),
                assignment(Some("gpt-5.6"), 1, "ultra", Some(&digest), None)
                    .effort
                    .as_ref()
                    .unwrap(),
                &report
            )
            .is_err()
        );
        assert!(
            verify_effort(
                Some("gpt-5.6-alias"),
                assignment(Some("gpt-5.6-alias"), 1, "high", Some(&digest), None)
                    .effort
                    .as_ref()
                    .unwrap(),
                &report
            )
            .is_err()
        );
        assert!(
            verify_effort(
                Some("gpt-5.6"),
                assignment(Some("gpt-5.6"), 1, "high", Some("stale"), None)
                    .effort
                    .as_ref()
                    .unwrap(),
                &report
            )
            .is_err()
        );
        assert!(
            verify_effort(
                Some("gpt-5.6"),
                assignment(Some("gpt-5.6"), 2, "high", Some(&digest), None)
                    .effort
                    .as_ref()
                    .unwrap(),
                &report
            )
            .is_err()
        );
    }

    #[test]
    fn asserted_effort_is_limited_to_recognized_values_and_opted_in_catalogs() {
        let report = success_report("0.1.0", "codex-cli 0.153.4".to_owned(), Vec::new());
        let digest = report.catalog_digest.clone();
        assert!(
            verify_effort(
                Some("unlisted-model"),
                assignment(
                    Some("unlisted-model"),
                    1,
                    "ultra",
                    Some(&digest),
                    Some("asserted")
                )
                .effort
                .as_ref()
                .unwrap(),
                &report
            )
            .is_ok()
        );
        assert!(
            verify_effort(
                Some("unlisted-model"),
                assignment(
                    Some("unlisted-model"),
                    1,
                    "extreme",
                    Some(&digest),
                    Some("asserted")
                )
                .effort
                .as_ref()
                .unwrap(),
                &report
            )
            .is_err()
        );
        assert!(
            verify_effort(
                Some("unlisted-model"),
                assignment(Some("unlisted-model"), 1, "high", None, Some("asserted"))
                    .effort
                    .as_ref()
                    .unwrap(),
                &report
            )
            .is_err()
        );
        assert!(report.accepts_asserted_effort.unwrap());
    }

    #[test]
    fn refresh_is_ttl_gated_and_missing_data_refreshes_immediately() {
        let report = capabilities();
        let now = Instant::now();
        assert!(report.refresh_due(None, now));
        assert!(!report.refresh_due(Some(now), now + Duration::from_secs(599)));
        assert!(report.refresh_due(Some(now), now + Duration::from_secs(600)));
    }

    #[test]
    fn oversized_unterminated_app_server_line_stops_at_stdout_bound() {
        struct RepeatingReader {
            remaining: usize,
            bytes_read: usize,
        }

        impl Read for RepeatingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = buffer.len().min(self.remaining);
                buffer[..count].fill(b'x');
                self.remaining -= count;
                self.bytes_read += count;
                Ok(count)
            }
        }

        let mut reader = RepeatingReader {
            remaining: 16 * MAX_STDOUT,
            bytes_read: 0,
        };
        let (sender, receiver) = mpsc::channel();
        read_lines_bounded(&mut reader, sender);

        assert_eq!(reader.bytes_read, MAX_STDOUT + 1);
        assert!(matches!(
            receiver.recv(),
            Ok(AppServerLine::Error(message))
                if message == "Codex model discovery exceeded 1 MiB"
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_uses_the_codex_version_and_app_server_model_catalog() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "tines-runner-codex-capabilities-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create fake Codex directory");
        let program = root.join("codex");
        fs::write(
            &program,
            r##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.153.4\n'
  exit 0
fi
if [ "$1" = "app-server" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
            *'"method":"model/list"'*'"cursor":"page-2"'*)
                printf '%s\n' '{"id":4,"result":{"data":[{"model":"gpt-5.4","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]}}'
                exit 0
                ;;
      *'"method":"model/list"'*)
        printf '%s\n' '{"id":3,"result":{"data":[{"model":"gpt-5.6","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"},{"reasoningEffort":"low"}]}],"nextCursor":"page-2"}}'
        ;;
    esac
  done
fi
exit 2
"##,
        )
        .expect("write fake Codex executable");
        let mut permissions = fs::metadata(&program)
            .expect("stat fake Codex executable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).expect("make fake Codex executable");

        let report = EffortCapabilities::discover_with_program(
            program.to_str().expect("fake Codex path is UTF-8"),
            "0.1.0",
        );

        assert_eq!(
            report.harness_version, "codex-cli 0.153.4",
            "discovery error: {:?}",
            report.discovery_error
        );
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].model, "gpt-5.6");
        assert_eq!(report.models[0].efforts, ["low", "high"]);
        assert_eq!(report.models[1].model, "gpt-5.4");
        assert_eq!(report.models[1].efforts, ["medium"]);
        assert_eq!(
            report.catalog_digest,
            "40fa35512849af8bfcbeaeba2e81bc6dce093228167a1621e107b3bd9f279182"
        );
        assert_eq!(report.discovery_error, None);

        fs::remove_dir_all(root).expect("remove fake Codex directory");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_runs_through_the_legacy_wrapper_prefix() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "tines-runner-codex-wrapper-capabilities-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create fake Codex wrapper directory");
        let wrapper = root.join("codex-wrapper");
        fs::write(
            &wrapper,
            "#!/bin/sh\n[ \"$1\" = \"--profile\" ] || exit 8\nshift\n[ \"$1\" = \"codex\" ] || exit 9\nshift\nif [ \"$1\" = \"--version\" ]; then printf 'wrapped-codex 1.2.3\\n'; exit 0; fi\nexit 10\n",
        )
        .expect("write fake Codex wrapper");
        let mut permissions = fs::metadata(&wrapper)
            .expect("stat fake Codex wrapper")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).expect("make wrapper executable");

        let report = EffortCapabilities::discover_with_wrapper(
            &[
                wrapper.to_string_lossy().into_owned(),
                "--profile".to_owned(),
            ],
            "0.1.0",
        );

        assert_eq!(report.harness_version, "wrapped-codex 1.2.3");
        assert!(report.discovery_error.is_some());
        assert!(report.supports_harness("codex"));
        fs::remove_dir_all(root).expect("remove fake Codex wrapper directory");
    }
}
