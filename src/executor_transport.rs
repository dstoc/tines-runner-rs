//! Daemon-side process transport for one local executor invocation.

use std::env;
use std::error::Error;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::raw::{c_char, c_int};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use crate::assignment::ResolvedAssignment;
use crate::config::{Config, ResolvedRunConfig, RunKeyDelivery, RunnerType, WorkspaceRetention};
use crate::execution_protocol::{
    EXECUTION_PROTOCOL_VERSION, ExecutionRequest, ExecutionRetentionPolicy, LocalExecutionPolicy,
    TinesExecutionContext, WorkspacePolicy,
};
use crate::executor_capabilities::ExecutorCapabilities;
use crate::logging::{FailureDisposition, FailureReporter};
use crate::process::{ProcessExit as ChildExit, ProcessIdentity, ProcessStream, SupervisedProcess};

const EXECUTOR_MODE: &str = "execute";
const CAPABILITIES_MODE: &str = "capabilities";
const MAX_CAPABILITIES_OUTPUT_BYTES: usize = 64 * 1024;
const CAPABILITIES_DEADLINE: Duration = Duration::from_secs(10);
const CAPABILITIES_TERMINATION_GRACE: Duration = Duration::from_secs(2);
const MAX_STDERR_DIAGNOSTIC_BYTES: usize = 32 * 1024;
const STDERR_TRUNCATION_MARKER: &str = "\n[executor stderr truncated]";

#[cfg(unix)]
unsafe extern "C" {
    fn access(pathname: *const c_char, mode: c_int) -> c_int;
}

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
                RunnerType::Custom => "custom".to_owned(),
            },
            custom_command: policy.custom_command.clone(),
            repository_checkout: policy.repository_checkout,
            workspace: WorkspacePolicy {
                parent: policy.workspace_parent.clone(),
            },
            retention: ExecutionRetentionPolicy {
                mode: retention.mode,
                max_age_hours: retention.max_age.as_secs() / 3_600,
                max_count: retention.max_count,
            },
        },
        assignment: {
            let mut assignment = resolved.assignment().clone();
            if policy.run_key_delivery == RunKeyDelivery::Environment {
                assignment.run_key = None;
            }
            assignment
        },
    }
}

/// A direct argv command and its required daemon-side working directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorTransport {
    argv: Vec<String>,
    executor_cwd: PathBuf,
    capabilities_transport_name: &'static str,
}

impl ExecutorTransport {
    pub fn from_resolved(config: &ResolvedRunConfig) -> Self {
        Self::new(config.executor.clone(), config.executor_cwd.clone())
    }

    /// Build the default capability-probe transport from the global config.
    /// An explicit capability command is kept separate from the run command.
    pub fn for_capabilities(config: &Config) -> Self {
        match &config.capabilities_executor {
            Some(argv) => Self::new_named(
                argv.clone(),
                config.executor_cwd.clone(),
                "[runner].capabilities_executor",
            ),
            None => Self::new(config.executor.clone(), config.executor_cwd.clone()),
        }
    }

    /// Build an assignment-specific capability-probe transport. When no
    /// capability command is resolved, use that assignment's normal executor.
    pub fn for_resolved_capabilities(config: &ResolvedRunConfig) -> Self {
        match &config.capabilities_executor {
            Some(argv) => Self::new_named(
                argv.clone(),
                config.executor_cwd.clone(),
                "[runner].capabilities_executor",
            ),
            None => Self::new(config.executor.clone(), config.executor_cwd.clone()),
        }
    }

    /// Name of the configuration field that selected this transport.
    pub fn config_source(&self) -> &'static str {
        self.capabilities_transport_name
    }

    /// Return a stable, sanitized identifier for grouping probe failures.
    pub fn diagnostic_identity(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.argv.hash(&mut hasher);
        self.executor_cwd.hash(&mut hasher);
        format!(
            "{}:{:016x}",
            self.capabilities_transport_name,
            hasher.finish()
        )
    }

    pub fn new(argv: Vec<String>, executor_cwd: impl Into<PathBuf>) -> Self {
        Self::new_named(argv, executor_cwd, "[runner].executor")
    }

    fn new_named(
        argv: Vec<String>,
        executor_cwd: impl Into<PathBuf>,
        capabilities_transport_name: &'static str,
    ) -> Self {
        Self {
            argv,
            executor_cwd: executor_cwd.into(),
            capabilities_transport_name,
        }
    }

    /// Run `<argv...> capabilities` and validate the bounded JSON document it returns.
    pub fn discover_capabilities(&self) -> Result<ExecutorCapabilities, ExecutorTransportError> {
        let program = self
            .argv
            .first()
            .filter(|program| !program.trim().is_empty())
            .ok_or_else(|| self.capabilities_error("command is empty"))?;
        let working_directory = validate_capabilities_cwd(&self.executor_cwd)
            .map_err(|_| self.capabilities_error("executor_cwd is invalid or inaccessible"))?;
        let mut command = Command::new(program);
        command
            .args(self.argv.iter().skip(1))
            .arg(CAPABILITIES_MODE)
            .current_dir(working_directory)
            .stdin(std::process::Stdio::null());
        command.env_clear();
        command.envs(safe_capabilities_environment(env::vars_os()));

        let process = SupervisedProcess::spawn_with_output(&mut command)
            .map_err(|_| self.capabilities_error("could not start command"))?;
        let mut stdout = Vec::new();
        let mut stdout_too_large = false;
        let mut stderr = BoundedStderr::new(capability_redaction_forms(&self.argv));
        let output_result = process.wait_timeout_with_output(
            CAPABILITIES_DEADLINE,
            CAPABILITIES_TERMINATION_GRACE,
            || false,
            |chunk| match chunk.stream {
                ProcessStream::Stdout => {
                    let remaining = MAX_CAPABILITIES_OUTPUT_BYTES.saturating_sub(stdout.len());
                    let accepted = chunk.bytes.len().min(remaining);
                    stdout.extend_from_slice(&chunk.bytes[..accepted]);
                    stdout_too_large |= accepted < chunk.bytes.len();
                }
                ProcessStream::Stderr => stderr.push(&chunk.bytes),
            },
            || {},
        );
        let stderr = non_empty(stderr.finish());
        let output = output_result.map_err(|_| {
            self.capabilities_error_with_stderr("could not wait for command", stderr.clone())
        })?;

        if output.timed_out {
            return Err(
                self.capabilities_error_with_stderr("capability discovery timed out", stderr)
            );
        }
        if output.exit != ChildExit::Code(0) {
            return Err(self.capabilities_error_with_stderr("command failed", stderr));
        }
        if stdout_too_large {
            return Err(
                self.capabilities_error_with_stderr("capability document exceeded 64 KiB", stderr)
            );
        }
        ExecutorCapabilities::parse(&stdout)
            .map_err(|_| self.capabilities_error_with_stderr("invalid capability document", stderr))
    }

    /// Discover capabilities and report probe failures with bounded, redacted
    /// local diagnostics. Repeated failures are summarized until recovery.
    pub fn discover_capabilities_reported(
        &self,
        failures: &FailureReporter,
    ) -> ExecutorCapabilities {
        let path = self.diagnostic_identity();
        match self.discover_capabilities() {
            Ok(capabilities) => {
                if let Some(reason) = capabilities.discovery_error.as_deref() {
                    let secret_forms = capability_redaction_forms(&self.argv);
                    let reason = redact_with_forms(reason, &secret_forms);
                    let reason = crate::diagnostic::format_bounded_diagnostic(
                        &reason,
                        crate::diagnostic::DIAGNOSTIC_EVENT_LIMIT,
                        crate::diagnostic::KeepPart::Suffix,
                    );
                    let signature = format!("{}: {reason}", self.config_source());
                    self.log_capability_failure(failures, &path, &signature, &reason, None);
                } else {
                    self.log_capability_recovery(failures, &path);
                    tracing::debug!(
                        transport = self.config_source(),
                        harnesses = ?capabilities.harnesses.keys().collect::<Vec<_>>(),
                        "executor capabilities discovered"
                    );
                }
                capabilities
            }
            Err(error) => {
                let (_, reason) = error
                    .capability_failure()
                    .expect("capability discovery returns a capability error");
                let diagnostic = error.diagnostic_stderr();
                self.log_capability_failure(
                    failures,
                    &path,
                    &error.failure_signature(),
                    reason,
                    diagnostic,
                );
                ExecutorCapabilities::unavailable(error.to_string())
            }
        }
    }

    fn log_capability_failure(
        &self,
        failures: &FailureReporter,
        path: &str,
        signature: &str,
        reason: &str,
        stderr: Option<&str>,
    ) {
        let diagnostic = stderr.map(|stderr| {
            crate::diagnostic::format_bounded_diagnostic(
                stderr,
                crate::diagnostic::DIAGNOSTIC_EVENT_LIMIT,
                crate::diagnostic::KeepPart::Suffix,
            )
        });
        match failures.record_failure(path, signature) {
            FailureDisposition::First => tracing::warn!(
                transport = self.config_source(),
                reason,
                diagnostic = diagnostic.as_deref(),
                "executor capability discovery failed"
            ),
            FailureDisposition::Repeated { failures } => tracing::debug!(
                transport = self.config_source(),
                reason,
                failure_count = failures,
                diagnostic = diagnostic.as_deref(),
                "executor capability discovery still failing"
            ),
        }
    }

    fn log_capability_recovery(&self, failures: &FailureReporter, path: &str) {
        if let Some(recovery) = failures.recovered(path) {
            tracing::info!(
                transport = self.config_source(),
                failure_count = recovery.failures,
                outage_seconds = recovery.outage.as_secs(),
                "executor capability discovery recovered"
            );
        }
    }

    fn capabilities_error(&self, reason: &'static str) -> ExecutorTransportError {
        self.capabilities_error_with_stderr(reason, None)
    }

    fn capabilities_error_with_stderr(
        &self,
        reason: &'static str,
        stderr: Option<String>,
    ) -> ExecutorTransportError {
        ExecutorTransportError::Capabilities {
            transport_name: self.capabilities_transport_name,
            reason,
            stderr,
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
        on_stdout: F,
    ) -> Result<ExecutorOutput, ExecutorTransportError>
    where
        F: FnMut(&[u8]),
    {
        self.run_with_callbacks(
            request,
            deadline,
            termination_grace,
            || false,
            || false,
            |_, _| Ok(()),
            on_stdout,
            || {},
        )
    }

    /// Run the executor while reporting its process identity and forwarding
    /// cancellation, shutdown, stdout, and quiet-period callbacks.
    #[allow(clippy::too_many_arguments)]
    pub fn run_with_callbacks<F, C, I, S, T>(
        &self,
        request: &ExecutionRequest,
        deadline: Duration,
        termination_grace: Duration,
        is_cancelled: C,
        is_interrupted: I,
        on_started: S,
        on_stdout: F,
        on_tick: T,
    ) -> Result<ExecutorOutput, ExecutorTransportError>
    where
        F: FnMut(&[u8]),
        C: FnMut() -> bool,
        I: FnMut() -> bool,
        S: FnMut(&ProcessIdentity, Instant) -> Result<(), String>,
        T: FnMut(),
    {
        self.run_with_callbacks_and_environment_key(
            request,
            None,
            deadline,
            termination_grace,
            is_cancelled,
            is_interrupted,
            on_started,
            on_stdout,
            on_tick,
        )
    }

    /// Run the executor and optionally deliver the run key in its environment.
    /// `request` remains the serialized stdin document, so environment delivery
    /// does not add a run key to that document.
    #[allow(clippy::too_many_arguments)]
    pub fn run_with_callbacks_and_environment_key<F, C, I, S, T>(
        &self,
        request: &ExecutionRequest,
        environment_run_key: Option<&str>,
        deadline: Duration,
        termination_grace: Duration,
        is_cancelled: C,
        is_interrupted: I,
        mut on_started: S,
        mut on_stdout: F,
        on_tick: T,
    ) -> Result<ExecutorOutput, ExecutorTransportError>
    where
        F: FnMut(&[u8]),
        C: FnMut() -> bool,
        I: FnMut() -> bool,
        S: FnMut(&ProcessIdentity, Instant) -> Result<(), String>,
        T: FnMut(),
    {
        request
            .validate()
            .map_err(|error| ExecutorTransportError::InvalidRequest(error.to_string()))?;
        let input = serde_json::to_vec(request)
            .map_err(|_| ExecutorTransportError::RequestSerialization)?;
        let mut redaction_request = request.clone();
        if redaction_request.assignment.run_key.is_none() {
            redaction_request.assignment.run_key = environment_run_key.map(str::to_owned);
        }
        let program = self
            .argv
            .first()
            .filter(|program| !program.trim().is_empty())
            .ok_or(ExecutorTransportError::MissingCommand)?;
        let working_directory = validate_executor_cwd(&self.executor_cwd, &redaction_request)?;

        let mut command = Command::new(program);
        command
            .args(self.argv.iter().skip(1))
            .arg(EXECUTOR_MODE)
            .current_dir(&working_directory);
        sanitize_executor_environment(&mut command, &redaction_request, environment_run_key);

        let process = SupervisedProcess::spawn_with_stdin_and_output(&mut command, input).map_err(
            |error| {
                ExecutorTransportError::Spawn(redact_text(&error.to_string(), &redaction_request))
            },
        )?;
        let execution_deadline = Instant::now() + deadline;
        if let Err(error) = on_started(process.identity(), execution_deadline) {
            let error = redact_text(&error, &redaction_request);
            let _ = process.terminate(termination_grace);
            return Err(ExecutorTransportError::Start(error));
        }
        let mut stderr = BoundedStderr::for_request(&redaction_request);
        let output_result = process.wait_timeout_with_output_or_shutdown(
            execution_deadline.saturating_duration_since(Instant::now()),
            termination_grace,
            is_cancelled,
            is_interrupted,
            |chunk| match chunk.stream {
                ProcessStream::Stdout => on_stdout(&chunk.bytes),
                ProcessStream::Stderr => stderr.push(&chunk.bytes),
            },
            on_tick,
        );
        let stderr = stderr.finish();
        let output = output_result.map_err(|error| ExecutorTransportError::Wait {
            error: redact_text(&error.to_string(), &redaction_request),
            stderr: non_empty(stderr.clone()),
        })?;

        if output.stdin_error.is_some()
            && !output.timed_out
            && !output.cancelled
            && !output.interrupted
        {
            return Err(ExecutorTransportError::RequestDelivery {
                stderr: non_empty(stderr),
            });
        }

        Ok(ExecutorOutput {
            exit: output.exit,
            timed_out: output.timed_out,
            cancelled: output.cancelled,
            interrupted: output.interrupted,
            stderr,
        })
    }
}

/// The executor process result. Stdout has already been delivered in chunks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorOutput {
    pub exit: ChildExit,
    pub timed_out: bool,
    pub cancelled: bool,
    pub interrupted: bool,
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
    validate_executor_cwd_search_access(path, request)?;
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

fn validate_capabilities_cwd(path: &Path) -> Result<PathBuf, ExecutorTransportError> {
    let metadata = fs::metadata(path).map_err(|_| {
        ExecutorTransportError::InvalidWorkingDirectory(format!(
            "configured executor_cwd {} is unavailable",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ExecutorTransportError::InvalidWorkingDirectory(format!(
            "configured executor_cwd {} is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        const X_OK: c_int = 1;
        let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            ExecutorTransportError::InvalidWorkingDirectory(
                "configured executor_cwd contains a null byte".to_owned(),
            )
        })?;
        if unsafe { access(path_c.as_ptr(), X_OK) } != 0 {
            return Err(ExecutorTransportError::InvalidWorkingDirectory(format!(
                "configured executor_cwd {} is inaccessible",
                path.display()
            )));
        }
    }
    fs::read_dir(path).map_err(|_| {
        ExecutorTransportError::InvalidWorkingDirectory(format!(
            "configured executor_cwd {} is inaccessible",
            path.display()
        ))
    })?;
    fs::canonicalize(path).map_err(|_| {
        ExecutorTransportError::InvalidWorkingDirectory(format!(
            "could not resolve configured executor_cwd {}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn validate_executor_cwd_search_access(
    path: &Path,
    request: &ExecutionRequest,
) -> Result<(), ExecutorTransportError> {
    const X_OK: c_int = 1;
    let path_bytes = path.as_os_str().as_bytes();
    let path_c = CString::new(path_bytes).map_err(|_| {
        ExecutorTransportError::InvalidWorkingDirectory(redact_text(
            "configured executor_cwd contains a null byte",
            request,
        ))
    })?;
    // access(X_OK) checks the permission needed to enter a directory without
    // changing the daemon's current working directory.
    if unsafe { access(path_c.as_ptr(), X_OK) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(ExecutorTransportError::InvalidWorkingDirectory(
        redact_text(
            &format!(
                "configured executor_cwd {} is inaccessible: {error}",
                path.display()
            ),
            request,
        ),
    ))
}

#[cfg(not(unix))]
fn validate_executor_cwd_search_access(
    _path: &Path,
    _request: &ExecutionRequest,
) -> Result<(), ExecutorTransportError> {
    Ok(())
}

fn sanitize_executor_environment(
    command: &mut Command,
    request: &ExecutionRequest,
    environment_run_key: Option<&str>,
) {
    command.env_clear();
    command.envs(safe_executor_environment(env::vars_os(), request));
    if let Some(run_key) = environment_run_key {
        command.env("TINES_API_KEY", run_key);
    }
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
    let restricted_values = secret_values(request);
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
    [
        "TINES_API_KEY",
        "TINES_API_URL",
        "TINES_RUNNER_TOKEN",
        "TYPESAFE_API_KEY",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn safe_capabilities_environment(
    environment: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    environment
        .filter(|(name, _)| !is_tines_credential_name(&name.to_string_lossy()))
        .collect()
}

fn secret_values(request: &ExecutionRequest) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(run_key) = request.assignment.run_key.as_ref()
        && !run_key.is_empty()
    {
        values.push(run_key.clone());
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

fn secret_forms(request: &ExecutionRequest) -> Vec<String> {
    redaction_forms(secret_values(request))
}

fn capability_redaction_forms(argv: &[String]) -> Vec<String> {
    let mut values = argv.iter().skip(1).cloned().collect::<Vec<_>>();
    values.extend(env::vars_os().filter_map(|(name, value)| {
        let name = name.to_string_lossy();
        is_sensitive_environment_name(&name).then(|| value.to_string_lossy().into_owned())
    }));
    redaction_forms(values)
}

fn redaction_forms(values: Vec<String>) -> Vec<String> {
    let mut forms = values
        .into_iter()
        .filter(|value| !value.is_empty())
        .flat_map(|secret| {
            let escaped =
                serde_json::to_string(&secret).expect("Rust strings always serialize to JSON");
            let rust_escaped = format!("{secret:?}");
            [
                secret,
                escaped[1..escaped.len() - 1].to_owned(),
                rust_escaped[1..rust_escaped.len() - 1].to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms.dedup();
    forms
}

fn redact_text(value: &str, request: &ExecutionRequest) -> String {
    redact_with_forms(value, &secret_forms(request))
}

fn redact_with_forms(value: &str, forms: &[String]) -> String {
    let mut redacted = value.to_owned();
    for secret in forms {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    redacted
}

struct BoundedStderr {
    secret_forms: Vec<String>,
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedStderr {
    fn for_request(request: &ExecutionRequest) -> Self {
        Self {
            secret_forms: secret_forms(request),
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn new(secret_forms: Vec<String>) -> Self {
        Self {
            secret_forms,
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
        diagnostic = redact_with_forms(&diagnostic, &self.secret_forms);
        let truncated = self.truncated || diagnostic.len() > MAX_STDERR_DIAGNOSTIC_BYTES;
        if truncated {
            let longest_secret = self.secret_forms.iter().map(String::len).max().unwrap_or(0);
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

fn non_empty(diagnostic: String) -> Option<String> {
    (!diagnostic.trim().is_empty()).then_some(diagnostic)
}

fn is_sensitive_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| name.contains(marker))
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
    Start(String),
    RequestDelivery {
        stderr: Option<String>,
    },
    Wait {
        error: String,
        stderr: Option<String>,
    },
    Capabilities {
        transport_name: &'static str,
        reason: &'static str,
        stderr: Option<String>,
    },
}

impl ExecutorTransportError {
    /// Bounded, redacted stderr captured before a transport failure, when any.
    pub fn diagnostic_stderr(&self) -> Option<&str> {
        match self {
            Self::RequestDelivery { stderr }
            | Self::Wait { stderr, .. }
            | Self::Capabilities { stderr, .. } => stderr.as_deref(),
            _ => None,
        }
    }

    /// Stable source and reason for suppressing repeated capability failures.
    pub fn failure_signature(&self) -> String {
        match self {
            Self::Capabilities {
                transport_name,
                reason,
                ..
            } => format!("{transport_name}: {reason}"),
            _ => self.to_string(),
        }
    }

    /// Configuration source and sanitized failure reason for capability logs.
    pub fn capability_failure(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Capabilities {
                transport_name,
                reason,
                ..
            } => Some((*transport_name, *reason)),
            _ => None,
        }
    }
}

impl fmt::Display for ExecutorTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => f.write_str("executor command must contain a program"),
            Self::InvalidWorkingDirectory(error) => f.write_str(error),
            Self::InvalidRequest(error) => write!(f, "invalid executor request: {error}"),
            Self::RequestSerialization => f.write_str("could not serialize executor request"),
            Self::Spawn(error) => write!(f, "could not start executor transport: {error}"),
            Self::Start(error) => write!(f, "could not initialize executor transport: {error}"),
            Self::RequestDelivery { .. } => {
                f.write_str("executor closed stdin before receiving the complete request")
            }
            Self::Wait { error, .. } => write!(f, "could not wait for executor transport: {error}"),
            Self::Capabilities {
                transport_name,
                reason,
                ..
            } => write!(
                f,
                "executor capabilities unavailable using {transport_name}: {reason}"
            ),
        }
    }
}

impl Error for ExecutorTransportError {}

#[cfg(test)]
mod tests {
    use super::{ExecutionRequest, ExecutorTransport, safe_executor_environment};
    use crate::logging::FailureReporter;
    use crate::logging::test_support::capture_events;
    use std::ffi::OsString;

    #[cfg(unix)]
    #[test]
    fn capability_document_discovery_errors_are_warned_locally() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "tines-runner-capability-error-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create temporary directory");
        let command = directory.join("capability-command");
        std::fs::write(
            &command,
            r##"#!/bin/sh
printf '%s\n' '{"version":1,"harnesses":{},"discovery_error":"Codex version probe failed"}'
"##,
        )
        .expect("write capability command");
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755))
            .expect("make capability command executable");
        let transport =
            ExecutorTransport::new(vec![command.to_string_lossy().into_owned()], &directory);
        let reporter = FailureReporter::default();

        let (capabilities, events) = {
            let mut capabilities = None;
            let events = capture_events(|| {
                capabilities = Some(transport.discover_capabilities_reported(&reporter));
            });
            (capabilities.expect("discover capability document"), events)
        };

        assert!(capabilities.discovery_error.is_some());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, tracing::Level::WARN);
        assert!(events[0].fields.contains("Codex version probe failed"));
        assert!(
            events[0]
                .fields
                .contains("executor capability discovery failed")
        );
        std::fs::remove_dir_all(directory).expect("remove temporary directory");
    }

    #[test]
    fn review_debug_escaped_stderr_secret_is_redacted() {
        let mut request = request();
        request.assignment.env[0].value = "prefix\u{8}suffix".to_owned();
        let diagnostic = format!("{:?}", request.assignment.env[0].value);
        let mut stderr = super::BoundedStderr::for_request(&request);
        stderr.push(diagnostic.as_bytes());
        let diagnostic = stderr.finish();
        assert!(
            !diagnostic.contains("prefix\\u{8}suffix"),
            "escaped secret remains in operator diagnostic: {diagnostic}"
        );
    }

    #[test]
    fn truncation_guard_accounts_for_rust_debug_escaped_secrets() {
        let mut request = request();
        request.assignment.env[0].value = format!("prefix{}suffix", "\u{8}".repeat(64));
        let debug_secret = format!("{:?}", request.assignment.env[0].value);
        let mut output = vec![b'x'; super::MAX_STDERR_DIAGNOSTIC_BYTES - 200];
        output.extend_from_slice(debug_secret.as_bytes());

        let mut stderr = super::BoundedStderr::for_request(&request);
        stderr.push(&output);
        let diagnostic = stderr.finish();
        assert!(diagnostic.contains(super::STDERR_TRUNCATION_MARKER));
        assert!(
            !diagnostic.contains("prefix"),
            "truncated secret prefix remains in operator diagnostic"
        );
    }

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
                    {"name": "BUILD_MODE", "value": "release-mode", "secret": false},
                    {"name": "BATCH_SIZE", "value": "2", "secret": false}
                ]
            }
        }))
        .expect("decode execution request")
    }

    #[test]
    fn filters_runner_credentials_and_secrets_without_removing_path_for_short_values() {
        let request = request();
        let environment = [
            ("PATH", "/usr/bin/2/bin"),
            ("TINES_API_KEY", "long-lived-runner-key"),
            ("TINES_API_URL", "https://tines.example.test"),
            ("tines_runner_token", "runner-token"),
            ("DEPLOY_TOKEN", "inherited-assignment-value"),
            ("BUILD_MODE", "inherited-build-mode"),
            ("BATCH_SIZE", "inherited-batch-size"),
            ("OTHER_MODE", "prefix-release-mode-suffix"),
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

        assert!(filtered.contains(&("PATH".to_owned(), "/usr/bin/2/bin".to_owned())));
        assert!(filtered.contains(&("SAFE_SETTING".to_owned(), "safe-value".to_owned())));
        assert!(filtered.contains(&(
            "OTHER_MODE".to_owned(),
            "prefix-release-mode-suffix".to_owned()
        )));
        assert!(filtered.iter().all(|(name, _)| {
            !name.eq_ignore_ascii_case("TINES_API_KEY")
                && !name.eq_ignore_ascii_case("TINES_API_URL")
                && !name.eq_ignore_ascii_case("TINES_RUNNER_TOKEN")
                && !name.eq_ignore_ascii_case("DEPLOY_TOKEN")
                && !name.eq_ignore_ascii_case("BUILD_MODE")
                && !name.eq_ignore_ascii_case("BATCH_SIZE")
        }));
        assert!(filtered.iter().all(|(_, value)| {
            !value.contains("ephemeral-run-key") && !value.contains("assignment-secret")
        }));
    }
}
