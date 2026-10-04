//! Long-running daemon polling, retry, ownership, and fencing.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::time::Duration;
use std::time::Instant;

use crate::assignment::PreparedAssignment;
use crate::config::Config;
use crate::executor_capabilities::ExecutorCapabilities;
use crate::executor_transport::ExecutorTransport;
use crate::protocol::client::{ErrorCategory, RunLogBuffer};
use crate::protocol::{
    RunnerCancellationAck, RunnerConcurrencyApplied, RunnerConcurrencyReport, RunnerPollRequest,
    RunnerPollResponse,
};
use crate::runner::{RunnerConnection, RunnerError};

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Local ownership and protocol state kept across poll failures.
///
/// The assignment executor owns child processes. It updates this inventory
/// only after a child starts and after cleanup completes, so a failed poll
/// cannot discard or interrupt a live run.
pub struct PollState {
    instance_id: String,
    owned_runs: BTreeSet<String>,
    cancellation_acks: BTreeMap<String, String>,
    declined_assignments: BTreeSet<String>,
    pending_assignments: BTreeMap<String, PreparedAssignment>,
    active_log_streams: BTreeMap<String, RunLogBuffer>,
    assignment_failures: BTreeMap<String, String>,
    allow_remote_concurrency: bool,
    local_ceiling: u32,
    effective_concurrency: u32,
    applied_concurrency: Option<RunnerConcurrencyApplied>,
    draining: bool,
    draining_poll_reported: bool,
    executor_transport: ExecutorTransport,
    executor_harness: String,
    executor_capabilities: Option<ExecutorCapabilities>,
    executor_capabilities_refreshed_at: Option<Instant>,
}

/// Result of reserving one server-delivered assignment in the local run set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssignmentAdmission {
    /// This assignment is new and now consumes one local concurrency slot.
    Accepted,
    /// This run is already tracked locally and must not be materialized again.
    AlreadyOwned,
    /// All slots under the effective local cap are in use.
    AtCapacity,
}

impl PollState {
    fn new(config: &Config) -> Self {
        Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            owned_runs: BTreeSet::new(),
            cancellation_acks: BTreeMap::new(),
            declined_assignments: BTreeSet::new(),
            pending_assignments: BTreeMap::new(),
            active_log_streams: BTreeMap::new(),
            assignment_failures: BTreeMap::new(),
            allow_remote_concurrency: config.allow_remote_concurrency,
            local_ceiling: config.max_concurrent as u32,
            effective_concurrency: config.max_concurrent as u32,
            applied_concurrency: None,
            draining: false,
            draining_poll_reported: false,
            executor_transport: ExecutorTransport::new(
                config.executor.clone(),
                config.executor_cwd.clone(),
            ),
            executor_harness: match config.runner_type {
                crate::config::RunnerType::Codex => "codex".to_owned(),
                crate::config::RunnerType::Custom => "custom".to_owned(),
            },
            executor_capabilities: None,
            executor_capabilities_refreshed_at: None,
        }
    }

    /// The unique ID sent for every poll during this daemon boot.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Mark a run as locally active after its child process starts.
    pub fn own_run(&mut self, run_id: impl Into<String>) {
        self.owned_runs.insert(run_id.into());
    }

    /// Whether a run is already reserved or running on the local daemon.
    pub fn owns_run(&self, run_id: &str) -> bool {
        self.owned_runs.contains(run_id)
    }

    /// Reserve a delivered run before resolving metadata or materializing its
    /// workspace. The reservation is included in the next `owned_runs` poll.
    pub fn admit_assignment(&mut self, run_id: impl Into<String>) -> AssignmentAdmission {
        let run_id = run_id.into();
        if self.owned_runs.contains(&run_id) {
            return AssignmentAdmission::AlreadyOwned;
        }
        if self.owned_runs.len() >= self.effective_concurrency as usize {
            return AssignmentAdmission::AtCapacity;
        }
        self.owned_runs.insert(run_id);
        AssignmentAdmission::Accepted
    }

    /// Number of runs that currently consume local concurrency slots.
    pub fn active_run_count(&self) -> usize {
        self.owned_runs.len()
    }

    /// Mark a run active and keep its shared log stream available for cancel handling.
    pub fn own_run_with_logs(&mut self, run_id: impl Into<String>, logs: RunLogBuffer) {
        let run_id = run_id.into();
        self.owned_runs.insert(run_id.clone());
        self.active_log_streams.insert(run_id, logs);
    }

    /// Remove a run after its child process and local resources are cleaned up.
    pub fn release_run(&mut self, run_id: &str) {
        self.owned_runs.remove(run_id);
        if let Some(logs) = self.active_log_streams.remove(run_id) {
            logs.stop_sending();
        }
    }

    /// Queue a cancellation acknowledgement after the executor has completed
    /// cancellation and workspace cleanup.
    pub fn acknowledge_cancellation(
        &mut self,
        run_id: impl Into<String>,
        token: impl Into<String>,
    ) {
        self.cancellation_acks.insert(run_id.into(), token.into());
    }

    /// Decline an assignment that the executor cannot launch. Tines will
    /// return its ID in `released_assignments` once it is safe to forget.
    pub fn decline_assignment(&mut self, run_id: impl Into<String>) {
        let run_id = run_id.into();
        self.owned_runs.remove(&run_id);
        self.pending_assignments.remove(&run_id);
        if let Some(logs) = self.active_log_streams.remove(&run_id) {
            logs.stop_sending();
        }
        self.declined_assignments.insert(run_id);
    }

    /// Set whether the daemon is draining and should receive no new assignments.
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
        if !draining {
            self.draining_poll_reported = false;
        }
    }

    /// Whether a successful poll has told Tines that this daemon is draining.
    pub fn draining_poll_reported(&self) -> bool {
        self.draining_poll_reported
    }

    /// Whether Tines still needs to acknowledge a declined assignment.
    pub fn has_pending_declines(&self) -> bool {
        !self.declined_assignments.is_empty()
    }

    /// Whether Tines still needs to accept a cancellation acknowledgement.
    pub fn has_pending_cancellation_acks(&self) -> bool {
        !self.cancellation_acks.is_empty()
    }

    /// Refresh the executor report when it expires, or before an effort-bearing
    /// assignment when `force` is true.
    pub fn refresh_executor_capabilities(&mut self, force: bool) -> &ExecutorCapabilities {
        let now = Instant::now();
        let expired = self
            .executor_capabilities
            .as_ref()
            .is_none_or(|capabilities| {
                capabilities.refresh_due(self.executor_capabilities_refreshed_at, now)
            });
        if force || expired {
            self.executor_capabilities = Some(
                self.executor_transport
                    .discover_capabilities()
                    .unwrap_or_else(|error| ExecutorCapabilities::unavailable(error.to_string())),
            );
            self.executor_capabilities_refreshed_at = Some(now);
        }
        self.executor_capabilities
            .as_ref()
            .expect("executor capabilities are discovered before use")
    }

    /// The local concurrency cap after applying the latest server instruction.
    pub fn effective_concurrency(&self) -> u32 {
        self.effective_concurrency
    }

    /// Resolved assignments delivered by Tines and not yet claimed by an executor.
    pub fn pending_assignments(&self) -> impl Iterator<Item = &PreparedAssignment> {
        self.pending_assignments.values()
    }

    /// Queue an assignment after its workspace has been materialized.
    pub fn queue_assignment(&mut self, assignment: PreparedAssignment) {
        self.pending_assignments
            .insert(assignment.assignment().run.id.clone(), assignment);
    }

    /// Remove a prepared assignment from the pending queue when an executor claims it.
    pub fn take_assignment(&mut self, run_id: &str) -> Option<PreparedAssignment> {
        self.pending_assignments.remove(run_id)
    }

    /// Record a local preparation failure for reporting through run finish.
    pub fn fail_assignment(&mut self, run_id: impl Into<String>, error: impl Into<String>) {
        let run_id = run_id.into();
        self.pending_assignments.remove(&run_id);
        self.assignment_failures.insert(run_id, error.into());
    }

    fn assignment_failures(&self) -> Vec<(String, String)> {
        self.assignment_failures
            .iter()
            .map(|(run_id, error)| (run_id.clone(), error.clone()))
            .collect()
    }

    fn acknowledge_assignment_failure(&mut self, run_id: &str) {
        self.assignment_failures.remove(run_id);
        self.owned_runs.remove(run_id);
    }

    fn request(&self) -> RunnerPollRequest {
        let cancellation_acks = (!self.cancellation_acks.is_empty()).then(|| {
            self.cancellation_acks
                .iter()
                .map(|(run_id, token)| RunnerCancellationAck {
                    run_id: run_id.clone(),
                    token: token.clone(),
                })
                .collect()
        });
        let declined_assignments = (!self.declined_assignments.is_empty())
            .then(|| self.declined_assignments.iter().cloned().collect());
        RunnerPollRequest {
            instance_id: Some(self.instance_id.clone()),
            owned_runs: self.owned_runs.iter().cloned().collect(),
            max_concurrent: Some(self.local_ceiling),
            concurrency_control: Some(RunnerConcurrencyReport {
                version: 1,
                allow_remote: self.allow_remote_concurrency,
                ceiling: self.local_ceiling,
                applied: self.applied_concurrency.clone(),
            }),
            cancellation_acks,
            declined_assignments,
            draining: Some(self.draining),
            env_delivery: Some(1),
            effort_capabilities: self.executor_capabilities.as_ref().map(|capabilities| {
                capabilities.effort_report(&self.executor_harness, crate::VERSION)
            }),
        }
    }

    fn observe(&mut self, response: &RunnerPollResponse) -> Result<(), PollError> {
        if let Some(control) = &response.concurrency_control
            && (control.version != 1
                || (control.available && (control.cap == 0 || control.cap > self.local_ceiling)))
        {
            return Err(PollError::InvalidConcurrencyInstruction);
        }
        for accepted in &response.cancellation_acks {
            if self.cancellation_acks.get(&accepted.run_id) == Some(&accepted.token) {
                self.cancellation_acks.remove(&accepted.run_id);
            }
        }
        for released in &response.released_assignments {
            self.declined_assignments.remove(released);
            self.release_run(released);
            if let Some(assignment) = self.pending_assignments.remove(released) {
                assignment.stop_log_delivery();
            }
            self.assignment_failures.remove(released);
        }
        for canceled in &response.cancels {
            self.pending_assignments.remove(canceled);
            self.assignment_failures.remove(canceled);
            if let Some(logs) = self.active_log_streams.get(canceled) {
                logs.stop_sending();
            }
            if let Some(assignment) = self.pending_assignments.remove(canceled) {
                assignment.stop_log_delivery();
            }
        }
        for request in &response.cancel_requests {
            if let Some(logs) = self.active_log_streams.get(&request.run_id) {
                logs.stop_sending();
            }
            if let Some(assignment) = self.pending_assignments.remove(&request.run_id) {
                assignment.stop_log_delivery();
            }
        }
        if let Some(control) = &response.concurrency_control {
            if control.available {
                self.effective_concurrency = control.cap;
                self.applied_concurrency = Some(RunnerConcurrencyApplied {
                    revision: control.revision,
                    cap: control.cap,
                });
            } else {
                self.effective_concurrency = self.local_ceiling;
                self.applied_concurrency = None;
            }
        }
        // Set by PollLoop after a successful request that included draining=true.
        Ok(())
    }
}

/// Runs the long-lived poll loop around an authenticated runner connection.
pub struct PollLoop {
    connection: RunnerConnection,
    interval: Duration,
    state: PollState,
}

impl PollLoop {
    pub fn new(connection: RunnerConnection, config: &Config) -> Self {
        Self {
            connection,
            interval: config.poll_interval,
            state: PollState::new(config),
        }
    }

    pub fn state(&self) -> &PollState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut PollState {
        &mut self.state
    }

    /// Poll until `should_continue` returns false. The handler receives every
    /// successful response and can queue resolved assignments or update local
    /// ownership after launching or cleaning up child processes. Retryable
    /// failures use exponential backoff and leave that ownership state untouched.
    pub fn run_with<H, C, S>(
        &mut self,
        handle_response: H,
        mut should_continue: C,
        sleep: S,
    ) -> Result<(), PollError>
    where
        H: FnMut(&RunnerPollResponse, &mut PollState),
        C: FnMut() -> bool,
        S: FnMut(Duration),
    {
        self.run_controlled(|_| {}, handle_response, |_| should_continue(), sleep)
    }

    /// Poll with a state update before every request and a state-aware stop condition.
    /// This lets shutdown report draining and continue until local ownership and
    /// pending decline acknowledgements have been settled.
    pub fn run_controlled<B, H, C, S>(
        &mut self,
        mut before_poll: B,
        mut handle_response: H,
        mut should_continue: C,
        mut sleep: S,
    ) -> Result<(), PollError>
    where
        B: FnMut(&mut PollState),
        H: FnMut(&RunnerPollResponse, &mut PollState),
        C: FnMut(&PollState) -> bool,
        S: FnMut(Duration),
    {
        let mut consecutive_failures = 0u32;
        loop {
            before_poll(&mut self.state);
            if !should_continue(&self.state) {
                break;
            }
            self.state.refresh_executor_capabilities(false);
            let request = self.state.request();
            match self.connection.poll(&request) {
                Ok(response) => {
                    if request.draining == Some(true) {
                        self.state.draining_poll_reported = true;
                    }
                    self.state.observe(&response)?;
                    handle_response(&response, &mut self.state);
                    for (run_id, error) in self.state.assignment_failures() {
                        let mut finish_failures = 0u32;
                        loop {
                            match self.connection.finish_failed_assignment(&run_id, &error) {
                                Ok(_) => {
                                    self.state.acknowledge_assignment_failure(&run_id);
                                    self.state.release_run(&run_id);
                                    tracing::error!(
                                        run_id,
                                        error,
                                        "assignment failed during preparation"
                                    );
                                    break;
                                }
                                Err(error) if is_retryable(&error) => {
                                    let delay = retry_delay(finish_failures);
                                    finish_failures = finish_failures.saturating_add(1);
                                    tracing::warn!(
                                        run_id,
                                        error = %error,
                                        backoff_seconds = delay.as_secs(),
                                        "reporting assignment failure failed; retrying"
                                    );
                                    sleep(delay);
                                }
                                Err(error) => return Err(PollError::Runner(error)),
                            }
                        }
                    }
                    consecutive_failures = 0;
                    before_poll(&mut self.state);
                    if should_continue(&self.state) {
                        wait_poll_interval(
                            self.interval,
                            &mut before_poll,
                            &mut should_continue,
                            &mut sleep,
                            &mut self.state,
                        );
                    }
                }
                Err(error) if is_retryable(&error) => {
                    let delay = retry_delay(consecutive_failures);
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(
                        error = %error,
                        backoff_seconds = delay.as_secs(),
                        "runner poll failed; retrying"
                    );
                    let delay = if self.state.draining {
                        delay.min(Duration::from_secs(1))
                    } else {
                        delay
                    };
                    wait_retry_delay(
                        delay,
                        &mut before_poll,
                        &mut should_continue,
                        &mut sleep,
                        &mut self.state,
                    );
                }
                Err(error) => return Err(PollError::Runner(error)),
            }
        }
        Ok(())
    }
}

fn wait_poll_interval<B, C, S>(
    duration: Duration,
    before_poll: &mut B,
    should_continue: &mut C,
    sleep: &mut S,
    state: &mut PollState,
) where
    B: FnMut(&mut PollState),
    C: FnMut(&PollState) -> bool,
    S: FnMut(Duration),
{
    let was_draining = state.draining;
    let mut remaining = duration;
    while !remaining.is_zero() {
        let interval = remaining.min(Duration::from_millis(100));
        sleep(interval);
        remaining = remaining.saturating_sub(interval);
        before_poll(state);
        if !should_continue(state) || (!was_draining && state.draining) {
            break;
        }
    }
}

fn wait_retry_delay<B, C, S>(
    duration: Duration,
    before_poll: &mut B,
    should_continue: &mut C,
    sleep: &mut S,
    state: &mut PollState,
) where
    B: FnMut(&mut PollState),
    C: FnMut(&PollState) -> bool,
    S: FnMut(Duration),
{
    let was_draining = state.draining;
    let mut remaining = duration;
    while !remaining.is_zero() {
        let interval = remaining.min(Duration::from_millis(100));
        sleep(interval);
        remaining = remaining.saturating_sub(interval);
        before_poll(state);
        if !should_continue(state) || (!was_draining && state.draining) {
            break;
        }
    }
}

fn is_retryable(error: &RunnerError) -> bool {
    matches!(error, RunnerError::Protocol(client) if client.category() == ErrorCategory::Retryable)
}

fn retry_delay(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.min(6);
    Duration::from_secs(1u64 << exponent).min(MAX_BACKOFF)
}

#[derive(Debug)]
pub enum PollError {
    Runner(RunnerError),
    InvalidConcurrencyInstruction,
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runner(error) => error.fmt(f),
            Self::InvalidConcurrencyInstruction => {
                f.write_str("Tines returned an invalid concurrency-control instruction")
            }
        }
    }
}

impl Error for PollError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runner(error) => Some(error),
            Self::InvalidConcurrencyInstruction => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PollLoop, PollState, retry_delay};
    use crate::config::Config;
    use crate::credentials::{CredentialStore, RunnerCredentials};
    use crate::effort::{EffortCapabilities, EffortModelCapability};
    use crate::executor_capabilities::{ExecutorCapabilities, ExecutorHarnessCapabilities};
    use crate::protocol::client::RunLogBuffer;
    use crate::protocol::{RunnerCancellationAck, RunnerPollResponse};
    use crate::runner::RunnerConnection;
    use serde_json::Value;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("tines-runner-poll-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn credentials_path(&self) -> PathBuf {
            self.0.join("credentials.toml")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(
        server_url: &str,
        credentials_file: &std::path::Path,
        allow_remote_concurrency: bool,
    ) -> Config {
        config_with_concurrency(server_url, credentials_file, allow_remote_concurrency, 3)
    }

    fn config_with_concurrency(
        server_url: &str,
        credentials_file: &std::path::Path,
        allow_remote_concurrency: bool,
        max_concurrent: usize,
    ) -> Config {
        Config::from_toml_str(&format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"poll-test\"\nexecutor = []\nexecutor_cwd = \"~\"\nmax_concurrent = {max_concurrent}\nallow_remote_concurrency = {allow_remote_concurrency}\n[storage]\ncredentials_file = {:?}\n",
            credentials_file
        ))
        .expect("valid poll config")
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = stream.read(&mut chunk).expect("read request");
            assert_ne!(count, 0, "client closed before request completed");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).expect("request headers");
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + 4 + length {
                return String::from_utf8(request).expect("UTF-8 request");
            }
        }
    }

    fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let server = thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, response_body)| {
                    let (mut stream, _) = listener.accept().expect("accept poll");
                    let request = read_request(&mut stream);
                    let (_, request_body) = request.split_once("\r\n\r\n").expect("request body");
                    let parsed = serde_json::from_str(request_body).expect("request JSON");
                    write!(
                        stream,
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        response_body.len(),
                        body = response_body
                    )
                    .expect("write mock response");
                    parsed
                })
                .collect()
        });
        (format!("http://{address}"), server)
    }

    fn poller(directory: &TestDirectory, url: &str, allow_remote_concurrency: bool) -> PollLoop {
        poller_with_concurrency(directory, url, allow_remote_concurrency, 3)
    }

    fn poller_with_concurrency(
        directory: &TestDirectory,
        url: &str,
        allow_remote_concurrency: bool,
        max_concurrent: usize,
    ) -> PollLoop {
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_poll", "poll-token"))
            .expect("store runner token");
        let config =
            config_with_concurrency(url, store.path(), allow_remote_concurrency, max_concurrent);
        let connection = RunnerConnection::connect(&config).expect("load runner connection");
        PollLoop::new(connection, &config)
    }

    #[test]
    fn stable_boot_id_assignments_and_local_concurrency_are_sent() {
        let directory = TestDirectory::new();
        let response = r#"{"assignments":[{"run":{"id":"arun_new","issue_id":"iss_new"},"prompt":"work","bundle":{},"run_key":"run-token","timeout_minutes":120}],"cancels":[],"concurrency_control":{"version":1,"available":true,"revision":7,"cap":2,"ceiling":3}}"#;
        let (url, server) = mock_server(vec![(200, response)]);
        let mut poller = poller(&directory, &url, true);
        let boot = poller.state().instance_id().to_owned();
        let assignment_count = Cell::new(0);
        let mut sleeps = Vec::new();
        poller
            .run_with(
                |reply, state| {
                    assignment_count.set(reply.assignments.len());
                    state.own_run(reply.assignments[0].run.id.clone());
                },
                || assignment_count.get() == 0,
                |delay| sleeps.push(delay),
            )
            .expect("poll assignment");
        let requests = server.join().expect("mock server");
        assert_eq!(assignment_count.get(), 1);
        assert_eq!(poller.state().effective_concurrency(), 2);
        assert_eq!(requests[0]["instance_id"], boot);
        assert_eq!(requests[0]["owned_runs"], serde_json::json!([]));
        assert_eq!(requests[0]["max_concurrent"], 3);
        assert_eq!(requests[0]["concurrency_control"]["version"], 1);
        assert_eq!(requests[0]["concurrency_control"]["ceiling"], 3);
        assert_eq!(requests[0]["concurrency_control"]["allow_remote"], true);
        assert_eq!(requests[0]["draining"], false);
        assert_eq!(requests[0]["env_delivery"], 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn supervisor_cancellation_stops_active_run_log_streams() {
        let directory = TestDirectory::new();
        let mut state = PollState::new(&config(
            "http://127.0.0.1:1",
            &directory.credentials_path(),
            true,
        ));
        let logs = RunLogBuffer::new();
        let requested_logs = RunLogBuffer::new();
        state.own_run_with_logs("arun_live", logs.clone());
        state.own_run_with_logs("arun_cancel_requested", requested_logs.clone());

        state
            .observe(&RunnerPollResponse {
                assignments: Vec::new(),
                cancels: vec!["arun_live".to_owned()],
                cancel_requests: vec![RunnerCancellationAck {
                    run_id: "arun_cancel_requested".to_owned(),
                    token: "cancel-token".to_owned(),
                }],
                cancellation_acks: Vec::new(),
                released_assignments: Vec::new(),
                concurrency_control: None,
            })
            .expect("valid cancellation response");

        assert!(logs.is_stopped());
        assert!(logs.is_cancelled());
        assert!(requested_logs.is_stopped());
        assert!(requested_logs.is_cancelled());
        state.release_run("arun_live");
        assert!(!state.owned_runs.contains("arun_live"));
    }

    #[test]
    fn draining_waits_for_tines_to_confirm_declined_assignments() {
        let directory = TestDirectory::new();
        let assignment = r#"{"assignments":[{"run":{"id":"arun_declined_on_shutdown","issue_id":"iss_shutdown"},"prompt":"work","bundle":{},"run_key":"run-key","timeout_minutes":30}],"cancels":[]}"#;
        let not_yet_released = r#"{"assignments":[],"cancels":[]}"#;
        let released = r#"{"assignments":[],"cancels":[],"released_assignments":["arun_declined_on_shutdown"]}"#;
        let (url, server) = mock_server(vec![
            (200, assignment),
            (200, not_yet_released),
            (200, released),
        ]);
        let mut poller = poller(&directory, &url, false);
        let shutdown = Cell::new(false);
        let handled = Cell::new(0);

        poller
            .run_controlled(
                |state| state.set_draining(shutdown.get()),
                |response, state| {
                    if handled.get() == 0 {
                        assert_eq!(response.assignments.len(), 1);
                        state.decline_assignment("arun_declined_on_shutdown");
                        shutdown.set(true);
                    }
                    handled.set(handled.get() + 1);
                },
                |state| {
                    !shutdown.get()
                        || state.active_run_count() > 0
                        || state.has_pending_declines()
                        || !state.draining_poll_reported()
                },
                |_| {},
            )
            .expect("poll until Tines releases the declined assignment");

        let requests = server.join().expect("mock server requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["draining"], false);
        assert_eq!(requests[1]["draining"], true);
        assert_eq!(requests[2]["draining"], true);
        for request in &requests[1..] {
            assert_eq!(
                request["declined_assignments"],
                serde_json::json!(["arun_declined_on_shutdown"])
            );
        }
        assert!(!poller.state().has_pending_declines());
        assert_eq!(handled.get(), 3);
    }

    #[test]
    fn fake_server_receives_the_cached_codex_effort_catalog_on_polls() {
        let directory = TestDirectory::new();
        let (url, server) = mock_server(vec![(200, r#"{"assignments":[],"cancels":[]}"#)]);
        let mut poller = poller(&directory, &url, false);
        let effort = EffortCapabilities {
            version: 1,
            daemon_version: "0.1.0".to_owned(),
            harness: "codex".to_owned(),
            harness_version: "codex-cli 0.153.4".to_owned(),
            catalog_digest: "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a"
                .to_owned(),
            models: vec![EffortModelCapability {
                model: "gpt-5.6".to_owned(),
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
            accepts_asserted_effort: Some(true),
            discovery_error: None,
        };
        poller.state_mut().executor_capabilities = Some(ExecutorCapabilities {
            version: 1,
            harnesses: BTreeMap::from([(
                "codex".to_owned(),
                ExecutorHarnessCapabilities {
                    version: "codex-cli 0.153.4".to_owned(),
                    effort: Some(effort),
                },
            )]),
            discovery_error: None,
        });
        poller.state_mut().executor_capabilities_refreshed_at = Some(Instant::now());
        let polls = Cell::new(0);

        poller
            .run_with(
                |_, _| polls.set(polls.get() + 1),
                || polls.get() == 0,
                |_| panic!("one poll should finish without sleeping"),
            )
            .expect("poll with effort capability report");

        let requests = server.join().expect("fake server request");
        assert_eq!(
            requests[0]["effort_capabilities"],
            serde_json::json!({
                "version": 1,
                "daemon_version": "0.1.0",
                "harness": "codex",
                "harness_version": "codex-cli 0.153.4",
                "catalog_digest": "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a",
                "models": [{ "model": "gpt-5.6", "efforts": ["low", "high"] }],
                "accepts_asserted_effort": true
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn executor_capability_cache_refreshes_on_expiry_and_before_effort_work() {
        let directory = TestDirectory::new();
        let counter = directory.0.join("capability-probes");
        let document = r#"{"version":1,"harnesses":{"codex":{"version":"codex-fake 0.1.0","effort":{"version":1,"daemon_version":"0.1.0","harness":"codex","harness_version":"codex-fake 0.1.0","catalog_digest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","models":[],"accepts_asserted_effort":true}}}}"#;
        let stub = directory.0.join("capability-executor");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '%s\\n' '{}'\n",
                counter.display(),
                document
            ),
        )
        .expect("write fake executor");
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))
            .expect("make fake executor executable");
        let argv = serde_json::to_string(&[stub.to_string_lossy().into_owned()])
            .expect("serialize executor command");
        let cwd = serde_json::to_string(&directory.0.to_string_lossy().as_ref())
            .expect("serialize executor cwd");
        let config = Config::from_toml_str(&format!(
            "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"capability-cache-test\"\nexecutor = {argv}\nexecutor_cwd = {cwd}\n[storage]\ncredentials_file = {:?}\n",
            directory.credentials_path()
        ))
        .expect("valid runner config");
        let mut state = PollState::new(&config);

        assert!(state.refresh_executor_capabilities(false).supports("codex"));
        assert!(state.refresh_executor_capabilities(false).supports("codex"));
        assert_eq!(fs::read(&counter).unwrap().len(), 1);

        state.refresh_executor_capabilities(true);
        assert_eq!(fs::read(&counter).unwrap().len(), 2);

        state.executor_capabilities_refreshed_at =
            Some(Instant::now() - Duration::from_secs(10 * 60));
        state.refresh_executor_capabilities(false);
        assert_eq!(fs::read(&counter).unwrap().len(), 3);
    }

    #[test]
    fn transient_poll_failure_retries_with_same_owned_run_state() {
        let directory = TestDirectory::new();
        let response = r#"{"assignments":[],"cancels":[]}"#;
        let (url, server) = mock_server(vec![
            (503, r#"{"error":{"code":"unavailable","message":"retry"}}"#),
            (200, response),
        ]);
        let mut poller = poller(&directory, &url, false);
        poller.state_mut().own_run("arun_live");
        let polls = Cell::new(0);
        let mut sleeps = Vec::new();
        poller
            .run_with(
                |_, _| polls.set(polls.get() + 1),
                || polls.get() == 0,
                |delay| sleeps.push(delay),
            )
            .expect("recover after server error");
        let requests = server.join().expect("mock server");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["owned_runs"], serde_json::json!(["arun_live"]));
        assert_eq!(requests[1]["owned_runs"], serde_json::json!(["arun_live"]));
        assert_eq!(requests[0]["instance_id"], requests[1]["instance_id"]);
        assert_eq!(sleeps.iter().sum::<Duration>(), Duration::from_secs(1));
    }

    #[test]
    fn outage_reconciliation_keeps_reservations_and_does_not_admit_duplicates() {
        let directory = TestDirectory::new();
        let first_delivery = r#"{"assignments":[{"run":{"id":"arun_a","issue_id":"iss_a"},"prompt":"a","bundle":{},"run_key":"key-a","timeout_minutes":30},{"run":{"id":"arun_b","issue_id":"iss_b"},"prompt":"b","bundle":{},"run_key":"key-b","timeout_minutes":30}],"cancels":[]}"#;
        let redelivered = r#"{"assignments":[{"run":{"id":"arun_a","issue_id":"iss_a"},"prompt":"a","bundle":{},"run_key":"key-a","timeout_minutes":30},{"run":{"id":"arun_b","issue_id":"iss_b"},"prompt":"b","bundle":{},"run_key":"key-b","timeout_minutes":30},{"run":{"id":"arun_c","issue_id":"iss_c"},"prompt":"c","bundle":{},"run_key":"key-c","timeout_minutes":30}],"cancels":[]}"#;
        let empty = r#"{"assignments":[],"cancels":[]}"#;
        let (url, server) = mock_server(vec![
            (200, first_delivery),
            (503, r#"{"error":{"code":"unavailable","message":"retry"}}"#),
            (200, redelivered),
            (200, empty),
        ]);
        let mut poller = poller_with_concurrency(&directory, &url, false, 2);
        let successful_polls = Cell::new(0);
        let accepted = Cell::new(0);
        let duplicates = Cell::new(0);
        let declined = Cell::new(0);
        poller
            .run_with(
                |response, state| {
                    for assignment in &response.assignments {
                        match state.admit_assignment(assignment.run.id.clone()) {
                            super::AssignmentAdmission::Accepted => {
                                accepted.set(accepted.get() + 1);
                            }
                            super::AssignmentAdmission::AlreadyOwned => {
                                duplicates.set(duplicates.get() + 1);
                            }
                            super::AssignmentAdmission::AtCapacity => {
                                declined.set(declined.get() + 1);
                                state.decline_assignment(assignment.run.id.clone());
                            }
                        }
                    }
                    successful_polls.set(successful_polls.get() + 1);
                },
                || successful_polls.get() < 3,
                |_| {},
            )
            .expect("recover and reconcile repeated assignment delivery");

        let requests = server.join().expect("mock server requests");
        assert_eq!(accepted.get(), 2);
        assert_eq!(duplicates.get(), 2);
        assert_eq!(declined.get(), 1);
        assert_eq!(poller.state().active_run_count(), 2);
        assert_eq!(
            requests[1]["owned_runs"],
            serde_json::json!(["arun_a", "arun_b"])
        );
        assert_eq!(
            requests[2]["owned_runs"],
            serde_json::json!(["arun_a", "arun_b"])
        );
        assert_eq!(
            requests[3]["owned_runs"],
            serde_json::json!(["arun_a", "arun_b"])
        );
        assert_eq!(
            requests[3]["declined_assignments"],
            serde_json::json!(["arun_c"])
        );
    }

    #[test]
    fn released_assignment_in_same_response_is_not_admitted() {
        let directory = TestDirectory::new();
        let delivered = r#"{"assignments":[{"run":{"id":"arun_released","issue_id":"iss_released"},"prompt":"work","bundle":{},"run_key":"run-key","timeout_minutes":30}],"cancels":[]}"#;
        let released = r#"{"assignments":[{"run":{"id":"arun_released","issue_id":"iss_released"},"prompt":"work","bundle":{},"run_key":"run-key","timeout_minutes":30}],"cancels":[],"released_assignments":["arun_released"]}"#;
        let (url, server) = mock_server(vec![(200, delivered), (200, released)]);
        let mut poller = poller(&directory, &url, false);
        let successful_polls = Cell::new(0);
        let accepted = Cell::new(0);
        poller
            .run_with(
                |response, state| {
                    for assignment in &response.assignments {
                        if response.released_assignments.contains(&assignment.run.id) {
                            continue;
                        }
                        if state.admit_assignment(assignment.run.id.clone())
                            == super::AssignmentAdmission::Accepted
                        {
                            accepted.set(accepted.get() + 1);
                        }
                    }
                    successful_polls.set(successful_polls.get() + 1);
                },
                || successful_polls.get() < 2,
                |_| {},
            )
            .expect("process released assignment response");
        let requests = server.join().expect("mock server requests");
        assert_eq!(accepted.get(), 1);
        assert_eq!(
            requests[1]["owned_runs"],
            serde_json::json!(["arun_released"])
        );
        assert_eq!(poller.state().active_run_count(), 0);
    }

    #[test]
    fn queued_assignment_failures_retry_and_report_each_run() {
        let directory = TestDirectory::new();
        let poll_response = r#"{"assignments":[],"cancels":[]}"#;
        let finish_response = r#"{"id":"arun_finished","status":"failed"}"#;
        let (url, server) = mock_server(vec![
            (200, poll_response),
            (503, r#"{"error":{"code":"unavailable","message":"retry"}}"#),
            (200, finish_response),
            (200, finish_response),
        ]);
        let mut poller = poller(&directory, &url, false);
        let polls = Cell::new(0);
        let mut sleeps = Vec::new();
        poller
            .run_with(
                |_, state| {
                    state.fail_assignment("arun_a", "first preparation error");
                    state.fail_assignment("arun_b", "second preparation error");
                    polls.set(polls.get() + 1);
                },
                || polls.get() == 0,
                |delay| sleeps.push(delay),
            )
            .expect("retry and report all queued assignment failures");

        let requests = server.join().expect("mock server");
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1]["error"], "first preparation error");
        assert_eq!(requests[2]["error"], "first preparation error");
        assert_eq!(requests[3]["error"], "second preparation error");
        assert_eq!(sleeps, [Duration::from_secs(1)]);
        assert!(poller.state().assignment_failures.is_empty());
    }

    #[test]
    fn non_retryable_finish_error_keeps_assignment_failure_queued() {
        let directory = TestDirectory::new();
        let poll_response = r#"{"assignments":[],"cancels":[]}"#;
        let (url, server) = mock_server(vec![
            (200, poll_response),
            (
                400,
                r#"{"error":{"code":"invalid_request","message":"invalid"}}"#,
            ),
        ]);
        let mut poller = poller(&directory, &url, false);
        let polls = Cell::new(0);
        let error = poller
            .run_with(
                |_, state| {
                    state.fail_assignment("arun_unconfirmed", "preparation error");
                    polls.set(polls.get() + 1);
                },
                || polls.get() == 0,
                |_| panic!("non-retryable finish errors must not back off"),
            )
            .expect_err("invalid finish request must stop the loop");

        assert!(error.to_string().contains("400"));
        assert_eq!(
            poller.state().assignment_failures.get("arun_unconfirmed"),
            Some(&"preparation error".to_owned())
        );
        assert_eq!(server.join().expect("mock server").len(), 2);
    }

    #[test]
    fn runner_conflict_is_fatal_and_does_not_retry() {
        let directory = TestDirectory::new();
        let (url, server) = mock_server(vec![(
            409,
            r#"{"error":{"code":"runner_conflict","message":"another daemon instance is serving this runner; this one has been superseded"}}"#,
        )]);
        let mut poller = poller(&directory, &url, false);
        let mut sleeps = Vec::new();
        let error = poller
            .run_with(|_, _| {}, || true, |delay| sleeps.push(delay))
            .expect_err("superseded daemon must exit");
        assert!(error.to_string().contains("superseded"));
        assert!(sleeps.is_empty());
        assert_eq!(server.join().expect("mock server").len(), 1);
    }

    #[test]
    fn remote_concurrency_instruction_is_acknowledged_on_the_next_poll() {
        let directory = TestDirectory::new();
        let response = r#"{"assignments":[],"cancels":[],"concurrency_control":{"version":1,"available":true,"revision":4,"cap":2,"ceiling":3}}"#;
        let (url, server) = mock_server(vec![(200, response), (200, response)]);
        let mut poller = poller(&directory, &url, true);
        let polls = Cell::new(0);
        poller
            .run_with(
                |_, _| polls.set(polls.get() + 1),
                || polls.get() < 2,
                |_| {},
            )
            .expect("apply remote cap");
        let requests = server.join().expect("mock server");
        assert_eq!(poller.state().effective_concurrency(), 2);
        assert_eq!(requests[1]["max_concurrent"], 3);
        assert_eq!(
            requests[1]["concurrency_control"]["applied"],
            serde_json::json!({"revision": 4, "cap": 2})
        );
    }

    #[test]
    fn remote_cap_limits_admission_and_cannot_exceed_the_local_ceiling() {
        let directory = TestDirectory::new();
        let assignments = r#"{"assignments":[{"run":{"id":"arun_a","issue_id":"iss_a"},"prompt":"a","bundle":{},"run_key":"key-a","timeout_minutes":30},{"run":{"id":"arun_b","issue_id":"iss_b"},"prompt":"b","bundle":{},"run_key":"key-b","timeout_minutes":30}],"cancels":[],"concurrency_control":{"version":1,"available":true,"revision":2,"cap":1,"ceiling":2}}"#;
        let empty = r#"{"assignments":[],"cancels":[],"concurrency_control":{"version":1,"available":true,"revision":2,"cap":1,"ceiling":2}}"#;
        let (url, server) = mock_server(vec![(200, assignments), (200, empty)]);
        let mut poller = poller_with_concurrency(&directory, &url, true, 2);
        let successful_polls = Cell::new(0);
        let accepted = Cell::new(0);
        let declined = Cell::new(0);
        poller
            .run_with(
                |response, state| {
                    for assignment in &response.assignments {
                        match state.admit_assignment(assignment.run.id.clone()) {
                            super::AssignmentAdmission::Accepted => {
                                accepted.set(accepted.get() + 1);
                            }
                            super::AssignmentAdmission::AlreadyOwned => {}
                            super::AssignmentAdmission::AtCapacity => {
                                declined.set(declined.get() + 1);
                                state.decline_assignment(assignment.run.id.clone());
                            }
                        }
                    }
                    successful_polls.set(successful_polls.get() + 1);
                },
                || successful_polls.get() < 2,
                |_| {},
            )
            .expect("apply remote cap under local ceiling");

        let requests = server.join().expect("mock server requests");
        assert_eq!(accepted.get(), 1);
        assert_eq!(declined.get(), 1);
        assert_eq!(poller.state().effective_concurrency(), 1);
        assert_eq!(poller.state().active_run_count(), 1);
        assert_eq!(requests[0]["max_concurrent"], 2);
        assert_eq!(requests[1]["max_concurrent"], 2);
        assert_eq!(requests[1]["owned_runs"], serde_json::json!(["arun_a"]));
        assert_eq!(
            requests[1]["declined_assignments"],
            serde_json::json!(["arun_b"])
        );
        assert_eq!(
            requests[1]["concurrency_control"]["applied"],
            serde_json::json!({"revision": 2, "cap": 1})
        );
    }

    #[test]
    fn remote_cap_above_local_ceiling_is_rejected() {
        let directory = TestDirectory::new();
        let response = r#"{"assignments":[],"cancels":[],"concurrency_control":{"version":1,"available":true,"revision":3,"cap":3,"ceiling":4}}"#;
        let (url, server) = mock_server(vec![(200, response)]);
        let mut poller = poller_with_concurrency(&directory, &url, true, 2);
        let error = poller
            .run_with(
                |_, _| panic!("invalid remote cap must not reach assignment handling"),
                || true,
                |_| panic!("invalid remote cap must not retry"),
            )
            .expect_err("remote cap must not exceed the local ceiling");
        assert!(error.to_string().contains("invalid concurrency-control"));
        assert_eq!(server.join().expect("mock server request").len(), 1);
        assert_eq!(poller.state().effective_concurrency(), 2);
    }

    #[test]
    fn cancellation_and_release_updates_preserve_active_ownership() {
        let directory = TestDirectory::new();
        let response = r#"{"assignments":[],"cancels":["arun_live"],"cancel_requests":[{"run_id":"arun_cancel_requested","token":"cancel-token"}],"cancellation_acks":[{"run_id":"arun_ack","token":"ack-token"}],"released_assignments":["arun_declined"]}"#;
        let (url, server) = mock_server(vec![(200, response)]);
        let mut poller = poller(&directory, &url, false);
        poller.state_mut().own_run("arun_live");
        poller
            .state_mut()
            .acknowledge_cancellation("arun_ack", "ack-token");
        poller.state_mut().decline_assignment("arun_declined");
        let polls = Cell::new(0);
        poller
            .run_with(
                |reply, _| {
                    assert_eq!(reply.cancels, ["arun_live"]);
                    assert_eq!(reply.cancel_requests[0].run_id, "arun_cancel_requested");
                    polls.set(polls.get() + 1);
                },
                || polls.get() == 0,
                |_| {},
            )
            .expect("process poll updates");
        let requests = server.join().expect("mock server");
        assert_eq!(
            requests[0]["cancellation_acks"],
            serde_json::json!([{"run_id":"arun_ack","token":"ack-token"}])
        );
        assert_eq!(
            requests[0]["declined_assignments"],
            serde_json::json!(["arun_declined"])
        );
        assert!(poller.state().cancellation_acks.is_empty());
        assert!(poller.state().declined_assignments.is_empty());
        assert!(poller.state().owned_runs.contains("arun_live"));
    }

    #[test]
    fn runner_token_rejection_is_fatal_and_does_not_retry() {
        let directory = TestDirectory::new();
        let (url, server) = mock_server(vec![(
            401,
            r#"{"error":{"code":"runner_token_invalid","message":"invalid token"}}"#,
        )]);
        let mut poller = poller(&directory, &url, false);
        let polls = Cell::new(0);
        let error = poller
            .run_with(
                |_, _| polls.set(polls.get() + 1),
                || polls.get() == 0,
                |_| panic!("authentication errors must not back off and retry"),
            )
            .expect_err("rejected runner token must stop the daemon");
        assert!(error.to_string().contains("rejected the runner token"));
        assert_eq!(server.join().expect("mock server").len(), 1);
    }

    #[test]
    fn retry_delay_grows_and_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(5), Duration::from_secs(32));
        assert_eq!(retry_delay(20), Duration::from_secs(60));
    }

    #[test]
    fn stable_instance_id_has_uuid_shape() {
        let config = config(
            "https://tines.example.test",
            PathBuf::from("/tmp/credentials").as_path(),
            false,
        );
        let state = PollState::new(&config);
        assert_eq!(state.instance_id().len(), 36);
        assert_eq!(
            state
                .instance_id()
                .bytes()
                .filter(|byte| *byte == b'-')
                .count(),
            4
        );
    }
}
