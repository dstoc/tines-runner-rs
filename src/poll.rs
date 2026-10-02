//! Long-running daemon polling, retry, ownership, and fencing.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::time::Duration;

use crate::assignment::ResolvedAssignment;
use crate::config::Config;
use crate::protocol::client::ErrorCategory;
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
    pending_assignments: BTreeMap<String, ResolvedAssignment>,
    allow_remote_concurrency: bool,
    local_ceiling: u32,
    effective_concurrency: u32,
    applied_concurrency: Option<RunnerConcurrencyApplied>,
    draining: bool,
}

impl PollState {
    fn new(config: &Config) -> Self {
        Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            owned_runs: BTreeSet::new(),
            cancellation_acks: BTreeMap::new(),
            declined_assignments: BTreeSet::new(),
            pending_assignments: BTreeMap::new(),
            allow_remote_concurrency: config.allow_remote_concurrency,
            local_ceiling: config.max_concurrent as u32,
            effective_concurrency: config.max_concurrent as u32,
            applied_concurrency: None,
            draining: false,
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

    /// Remove a run after its child process and local resources are cleaned up.
    pub fn release_run(&mut self, run_id: &str) {
        self.owned_runs.remove(run_id);
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
        self.pending_assignments.remove(&run_id);
        self.declined_assignments.insert(run_id);
    }

    /// Set the flag used by later graceful-shutdown integration.
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }

    /// The local concurrency cap after applying the latest server instruction.
    pub fn effective_concurrency(&self) -> u32 {
        self.effective_concurrency
    }

    /// Resolved assignments delivered by Tines and not yet claimed by an executor.
    pub fn pending_assignments(&self) -> impl Iterator<Item = &ResolvedAssignment> {
        self.pending_assignments.values()
    }

    /// Queue an assignment after its match context and config have resolved.
    pub fn queue_assignment(&mut self, assignment: ResolvedAssignment) {
        self.pending_assignments
            .insert(assignment.assignment().run.id.clone(), assignment);
    }

    /// Remove a resolved assignment from the pending queue when an executor claims it.
    pub fn take_assignment(&mut self, run_id: &str) -> Option<ResolvedAssignment> {
        self.pending_assignments.remove(run_id)
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
            self.pending_assignments.remove(released);
        }
        for canceled in &response.cancels {
            self.pending_assignments.remove(canceled);
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
        mut handle_response: H,
        mut should_continue: C,
        mut sleep: S,
    ) -> Result<(), PollError>
    where
        H: FnMut(&RunnerPollResponse, &mut PollState),
        C: FnMut() -> bool,
        S: FnMut(Duration),
    {
        let mut consecutive_failures = 0u32;
        while should_continue() {
            let request = self.state.request();
            match self.connection.poll(&request) {
                Ok(response) => {
                    self.state.observe(&response)?;
                    handle_response(&response, &mut self.state);
                    consecutive_failures = 0;
                    if should_continue() {
                        sleep(self.interval);
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
                    sleep(delay);
                }
                Err(error) => return Err(PollError::Runner(error)),
            }
        }
        Ok(())
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
    use crate::runner::RunnerConnection;
    use serde_json::Value;
    use std::cell::Cell;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

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
        Config::from_toml_str(&format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"poll-test\"\nmax_concurrent = 3\nallow_remote_concurrency = {allow_remote_concurrency}\n[storage]\ncredentials_file = {:?}\n",
            credentials_file,
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
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_poll", "poll-token"))
            .expect("store runner token");
        let config = config(url, store.path(), allow_remote_concurrency);
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
        assert!(sleeps.is_empty());
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
        assert_eq!(sleeps, [Duration::from_secs(1)]);
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
