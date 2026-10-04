//! Daemon-side process transport for one local executor invocation.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::assignment::ResolvedAssignment;
use crate::config::{ResolvedRunConfig, RunnerType, WorkspaceRetention};
use crate::execution_protocol::{
    EXECUTION_PROTOCOL_VERSION, ExecutionRequest, ExecutionRetentionPolicy, LocalExecutionPolicy,
    TinesExecutionContext, WorkspacePolicy,
};
use crate::process::{ProcessExit as ChildExit, ProcessStream, SupervisedProcess};

const EXECUTOR_MODE: &str = "execute";
const MAX_STDERR_DIAGNOSTIC_BYTES: usize = 32 * 1024;
const STDERR_TRUNCATION_MARKER: &str = "\n[executor stderr truncated]";

/// Build the complete executor request from an assignment after daemon-side
/// matching has selected its effective policy.
pub fn execution_request(
    resolved: &ResolvedAssignment,
    api_url: &str,
    retention: &WorkspaceRetention,
) -> ExecutionRequest {
    let policy = &resolved.resolution().config;
    ExecutionRequest {
        version: EXECUTION_PROTOCOL_VERSION,
        tines: TinesExecutionContext {
            api_url: api_url.to_owned(),
        },
        execution: LocalExecutionPolicy {
            harness: match policy.runner_type {
                RunnerType::Codex => "codex".to_owned(),
            },
            workspace: WorkspacePolicy {
                parent: policy.workspace_parent.clone(),
            },
            retention: ExecutionRetentionPolicy {
                mode: retention.mode,
                max_age_hours: retention.max_age.as_secs() / 3_600,
                max_count: retention.max_count,
            },
        },
        assignment: resolved.assignment().clone(),
    }
}

/// A direct argv command and its required daemon-side working directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorTransport {
    argv: Vec<String>,
    executor_cwd: PathBuf,
}

impl ExecutorTransport {
    pub fn from_resolved(config: &ResolvedRunConfig) -> Self {
        Self::new(config.executor.clone(), config.executor_cwd.clone())
    }

    pub fn new(argv: Vec<String>, executor_cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            executor_cwd: executor_cwd.into(),
        }
    }

    /// Launch `<argv...> execute`, deliver one JSON document on stdin, and
    /// stream stdout chunks to the caller. The executor's stderr is retained
    /// as a bounded, secret-redacted operator diagnostic.
    pub fn run<F>(
        &self,
        request: &ExecutionRequest,
        deadline: Duration,
        termination_grace: Duration,
        mut on_stdout: F,
    ) -> Result<ExecutorOutput, ExecutorTransportError>
    where
        F: FnMut(&[u8]),
    {
        request
            .validate()
            .map_err(|error| ExecutorTransportError::InvalidRequest(error.to_string()))?;
        let input = serde_json::to_vec(request)
            .map_err(|_| ExecutorTransportError::RequestSerialization)?;
        let program = self
            .argv
            .first()
            .filter(|program| !program.trim().is_empty())
            .ok_or(ExecutorTransportError::MissingCommand)?;
        let working_directory = validate_executor_cwd(&self.executor_cwd, request)?;

        let mut command = Command::new(program);
        command
            .args(self.argv.iter().skip(1))
            .arg(EXECUTOR_MODE)
            .current_dir(&working_directory);
        sanitize_executor_environment(&mut command, request);

        let execution_deadline = Instant::now() + deadline;
        let process = SupervisedProcess::spawn_with_stdin_and_output(&mut command, input).map_err(
            |error| ExecutorTransportError::Spawn(redact_text(&error.to_string(), request)),
        )?;
        let mut stderr = BoundedStderr::new(request);
        let output = process
            .wait_timeout_with_output(
                execution_deadline.saturating_duration_since(Instant::now()),
                termination_grace,
                || false,
                |chunk| match chunk.stream {
                    ProcessStream::Stdout => on_stdout(&chunk.bytes),
                    ProcessStream::Stderr => stderr.push(&chunk.bytes),
                },
                || {},
            )
            .map_err(|error| {
                ExecutorTransportError::Wait(redact_text(&error.to_string(), request))
            })?;

        if output.stdin_error.is_some() && !output.timed_out {
            return Err(ExecutorTransportError::RequestDelivery);
        }

        Ok(ExecutorOutput {
            exit: output.exit,
            timed_out: output.timed_out,
            stderr: stderr.finish(),
        })
    }
}

/// The executor process result. Stdout has already been delivered in chunks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorOutput {
    pub exit: ChildExit,
    pub timed_out: bool,
    pub stderr: String,
}

fn validate_executor_cwd(
    path: &Path,
    request: &ExecutionRequest,
) -> Result<PathBuf, ExecutorTransportError> {
    let metadata = fs::metadata(path).map_err(|error| {
        ExecutorTransportError::InvalidWorkingDirectory(redact_text(
            &format!(
                "configured executor_cwd {} is unavailable: {error}",
                path.display()
            ),
            request,
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ExecutorTransportError::InvalidWorkingDirectory(
            redact_text(
                &format!(
                    "configured executor_cwd {} is not a directory",
                    path.display()
                ),
                request,
            ),
        ));
    }
    fs::read_dir(path).map_err(|error| {
        ExecutorTransportError::InvalidWorkingDirectory(redact_text(
            &format!(
                "configured executor_cwd {} is inaccessible: {error}",
                path.display()
            ),
            request,
        ))
    })?;
    fs::canonicalize(path).map_err(|error| {
        ExecutorTransportError::InvalidWorkingDirectory(redact_text(
            &format!(
                "could not resolve configured executor_cwd {}: {error}",
                path.display()
            ),
            request,
        ))
    })
}

fn sanitize_executor_environment(command: &mut Command, request: &ExecutionRequest) {
    command.env_clear();
    command.envs(safe_executor_environment(env::vars_os(), request));
}

fn safe_executor_environment(
    environment: impl Iterator<Item = (OsString, OsString)>,
    request: &ExecutionRequest,
) -> Vec<(OsString, OsString)> {
    let assignment_names = request
        .assignment
        .env
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    let restricted_values = assignment_values(request);
    environment
        .filter(|(name, value)| {
            let name = name.to_string_lossy();
            let value = value.to_string_lossy();
            !is_tines_credential_name(&name)
                && !assignment_names
                    .iter()
                    .any(|assignment_name| name.eq_ignore_ascii_case(assignment_name))
                && !restricted_values
                    .iter()
                    .any(|restricted| value.contains(restricted))
        })
        .collect()
}

fn is_tines_credential_name(name: &str) -> bool {
    ["TINES_API_KEY", "TINES_API_URL", "TINES_RUNNER_TOKEN"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn secret_values(request: &ExecutionRequest) -> Vec<String> {
    let mut values = Vec::new();
    if !request.assignment.run_key.is_empty() {
        values.push(request.assignment.run_key.clone());
    }
    values.extend(
        request
            .assignment
            .env
            .iter()
            .filter(|entry| entry.secret && !entry.value.is_empty())
            .map(|entry| entry.value.clone()),
    );
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

fn assignment_values(request: &ExecutionRequest) -> Vec<String> {
    let mut values = vec![request.assignment.run_key.clone()];
    values.extend(
        request
            .assignment
            .env
            .iter()
            .map(|entry| entry.value.clone()),
    );
    values.retain(|value| !value.is_empty());
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

fn secret_forms(request: &ExecutionRequest) -> Vec<String> {
    let mut forms = secret_values(request)
        .into_iter()
        .flat_map(|secret| {
            let escaped =
                serde_json::to_string(&secret).expect("Rust strings always serialize to JSON");
            [secret, escaped[1..escaped.len() - 1].to_owned()]
        })
        .collect::<Vec<_>>();
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms.dedup();
    forms
}

fn redact_text(value: &str, request: &ExecutionRequest) -> String {
    let mut redacted = value.to_owned();
    for secret in secret_forms(request) {
        redacted = redacted.replace(&secret, "[REDACTED]");
    }
    redacted
}

struct BoundedStderr<'a> {
    request: &'a ExecutionRequest,
    bytes: Vec<u8>,
    truncated: bool,
}

impl<'a> BoundedStderr<'a> {
    fn new(request: &'a ExecutionRequest) -> Self {
        Self {
            request,
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let remaining = MAX_STDERR_DIAGNOSTIC_BYTES.saturating_sub(self.bytes.len());
        let accepted = chunk.len().min(remaining);
        self.bytes.extend_from_slice(&chunk[..accepted]);
        self.truncated |= accepted < chunk.len();
    }

    fn finish(self) -> String {
        let mut diagnostic = String::from_utf8_lossy(&self.bytes).into_owned();
        diagnostic = redact_text(&diagnostic, self.request);
        let truncated = self.truncated || diagnostic.len() > MAX_STDERR_DIAGNOSTIC_BYTES;
        if truncated {
            let longest_secret = secret_forms(self.request)
                .iter()
                .map(String::len)
                .max()
                .unwrap_or(0);
            let end = if self.truncated {
                diagnostic.len().saturating_sub(longest_secret)
            } else {
                diagnostic.len()
            };
            diagnostic.truncate(previous_char_boundary(&diagnostic, end));
            let available =
                MAX_STDERR_DIAGNOSTIC_BYTES.saturating_sub(STDERR_TRUNCATION_MARKER.len());
            diagnostic.truncate(previous_char_boundary(&diagnostic, available));
            diagnostic.push_str(STDERR_TRUNCATION_MARKER);
        }
        diagnostic
    }
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Errors produced before or during the executor transport.
#[derive(Debug)]
pub enum ExecutorTransportError {
    MissingCommand,
    InvalidWorkingDirectory(String),
    InvalidRequest(String),
    RequestSerialization,
    Spawn(String),
    RequestDelivery,
    Wait(String),
}

impl fmt::Display for ExecutorTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => f.write_str("executor command must contain a program"),
            Self::InvalidWorkingDirectory(error) => f.write_str(error),
            Self::InvalidRequest(error) => write!(f, "invalid executor request: {error}"),
            Self::RequestSerialization => f.write_str("could not serialize executor request"),
            Self::Spawn(error) => write!(f, "could not start executor transport: {error}"),
            Self::RequestDelivery => {
                f.write_str("executor closed stdin before receiving the complete request")
            }
            Self::Wait(error) => write!(f, "could not wait for executor transport: {error}"),
        }
    }
}

impl Error for ExecutorTransportError {}

#[cfg(test)]
mod tests {
    use super::{ExecutionRequest, safe_executor_environment};
    use std::ffi::OsString;

    fn request() -> ExecutionRequest {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "tines": {"api_url": "https://tines.example.test"},
            "execution": {
                "harness": "codex",
                "workspace": {"parent": "/workspaces"},
                "retention": {"mode": "never", "max_age_hours": 72, "max_count": 20}
            },
            "assignment": {
                "run": {"id": "arun_env", "issue_id": "iss_env"},
                "prompt": "run fixture",
                "bundle": {},
                "run_key": "ephemeral-run-key",
                "timeout_minutes": 5,
                "env": [
                    {"name": "DEPLOY_TOKEN", "value": "assignment-secret", "secret": true},
                    {"name": "BUILD_MODE", "value": "secret-build-mode", "secret": false}
                ]
            }
        }))
        .expect("decode execution request")
    }

    #[test]
    fn filters_runner_and_assignment_credentials_from_executor_environment() {
        let request = request();
        let environment = [
            ("PATH", "/usr/bin"),
            ("TINES_API_KEY", "long-lived-runner-key"),
            ("TINES_API_URL", "https://tines.example.test"),
            ("tines_runner_token", "runner-token"),
            ("DEPLOY_TOKEN", "inherited-assignment-value"),
            ("BUILD_MODE", "inherited-build-mode"),
            ("OTHER_MODE", "prefix-secret-build-mode-suffix"),
            ("OTHER_SETTING", "contains-assignment-secret"),
            ("SAFE_SETTING", "safe-value"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));

        let filtered = safe_executor_environment(environment, &request);
        let filtered = filtered
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<Vec<_>>();

        assert!(filtered.contains(&("PATH".to_owned(), "/usr/bin".to_owned())));
        assert!(filtered.contains(&("SAFE_SETTING".to_owned(), "safe-value".to_owned())));
        assert!(filtered.iter().all(|(name, _)| {
            !name.eq_ignore_ascii_case("TINES_API_KEY")
                && !name.eq_ignore_ascii_case("TINES_API_URL")
                && !name.eq_ignore_ascii_case("TINES_RUNNER_TOKEN")
                && !name.eq_ignore_ascii_case("DEPLOY_TOKEN")
                && !name.eq_ignore_ascii_case("BUILD_MODE")
        }));
        assert!(filtered.iter().all(|(_, value)| {
            !value.contains("ephemeral-run-key")
                && !value.contains("assignment-secret")
                && !value.contains("secret-build-mode")
        }));
    }
}
