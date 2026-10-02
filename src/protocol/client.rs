//! Direct HTTP client for the Tines local-runner protocol.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client as HttpClient, RequestBuilder};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::protocol::{
    AppendRunLogRequest, AppendRunLogResponse, FinishRunRequest, FinishRunResponse,
    IssueDetailResponse, RegisterRunnerRequest, RunnerPollRequest, RunnerPollResponse,
    RunnerTokenResponse,
};

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCategory {
    Authentication,
    Fencing,
    Retryable,
    Protocol,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication => f.write_str("authentication"),
            Self::Fencing => f.write_str("fencing/conflict"),
            Self::Retryable => f.write_str("retryable transport/server"),
            Self::Protocol => f.write_str("protocol/client"),
        }
    }
}

/// An HTTP failure that never stores a request header, response body, or URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientError {
    category: ErrorCategory,
    status: Option<StatusCode>,
}

impl ClientError {
    fn local_protocol_error() -> Self {
        Self {
            category: ErrorCategory::Protocol,
            status: None,
        }
    }

    fn transport_error() -> Self {
        Self {
            category: ErrorCategory::Retryable,
            status: None,
        }
    }

    fn http_status(status: StatusCode) -> Self {
        Self {
            category: classify_http_status(status),
            status: Some(status),
        }
    }

    fn http_response(status: StatusCode, body: &serde_json::Value) -> Self {
        // The current server uses 409 for both takeover fencing and a
        // retryable policy-reconciliation race. The message is the only
        // distinction in that protocol response.
        let retryable_policy_race = status == StatusCode::CONFLICT
            && body["error"]["message"].as_str().is_some_and(|message| {
                message == "runner policy changed during poll reconciliation; retry the poll"
            });
        Self {
            category: if retryable_policy_race {
                ErrorCategory::Retryable
            } else {
                classify_http_status(status)
            },
            status: Some(status),
        }
    }

    pub fn category(self) -> ErrorCategory {
        self.category
    }

    pub fn status(self) -> Option<StatusCode> {
        self.status
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(
                f,
                "Tines HTTP request failed with status {status} ({})",
                self.category
            ),
            None => write!(f, "Tines request failed ({})", self.category),
        }
    }
}

impl std::error::Error for ClientError {}

/// Classify an HTTP status without retaining any response content.
pub fn classify_http_status(status: StatusCode) -> ErrorCategory {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        ErrorCategory::Authentication
    } else if matches!(status, StatusCode::CONFLICT | StatusCode::LOCKED) {
        ErrorCategory::Fencing
    } else if status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS
        )
    {
        ErrorCategory::Retryable
    } else {
        ErrorCategory::Protocol
    }
}

#[derive(Clone)]
pub struct Client {
    base_url: Url,
    http: HttpClient,
    request_timeout: Duration,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        Self::with_timeout(base_url, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Create a client with a total request timeout capped at 120 seconds.
    pub fn with_timeout(base_url: &str, timeout: Duration) -> Result<Self, ClientError> {
        let mut base_url = Url::parse(base_url).map_err(|_| ClientError::local_protocol_error())?;
        if !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(ClientError::local_protocol_error());
        }
        base_url.set_query(None);
        base_url.set_fragment(None);

        let request_timeout = timeout.min(MAX_REQUEST_TIMEOUT);
        let http = HttpClient::builder()
            .timeout(request_timeout)
            // Credentials must remain scoped to the configured Tines host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClientError::local_protocol_error())?;

        Ok(Self {
            base_url,
            http,
            request_timeout,
        })
    }

    pub fn register_runner(
        &self,
        user_api_key: &str,
        request: &RegisterRunnerRequest,
    ) -> Result<RunnerTokenResponse, ClientError> {
        self.post_json(&["runners", "register"], user_api_key, request)
    }

    /// Check a runner token without letting the poll endpoint deliver work.
    ///
    /// The current Tines API authenticates poll requests before parsing their
    /// JSON body. This sends malformed JSON and accepts only the API's
    /// `invalid_json` response. The handler returns before poll processing, so
    /// this request cannot claim an assignment.
    pub fn verify_runner_token(
        &self,
        runner_id: &str,
        runner_token: &str,
    ) -> Result<(), ClientError> {
        let url = self.endpoint(&["runners", runner_id, "poll"])?;
        let request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_LENGTH, "1")
            .body("{");
        let response = self.send(request, runner_token)?;
        let status = response.status();
        if status != StatusCode::BAD_REQUEST {
            return Err(ClientError::http_status(status));
        }

        let body = response
            .json::<serde_json::Value>()
            .map_err(|_| ClientError::local_protocol_error())?;
        if body["error"]["code"] == "invalid_json" {
            Ok(())
        } else {
            Err(ClientError::http_status(status))
        }
    }

    pub fn poll_runner(
        &self,
        runner_id: &str,
        runner_token: &str,
        request: &RunnerPollRequest,
    ) -> Result<RunnerPollResponse, ClientError> {
        self.post_json(&["runners", runner_id, "poll"], runner_token, request)
    }

    pub fn append_run_log(
        &self,
        run_id: &str,
        runner_token: &str,
        request: &AppendRunLogRequest,
    ) -> Result<AppendRunLogResponse, ClientError> {
        self.post_json(&["runs", run_id, "logs"], runner_token, request)
    }

    /// Append a run log before an absolute deadline. This keeps a blocked
    /// start-log request within the time left for the harness.
    pub fn append_run_log_until(
        &self,
        run_id: &str,
        runner_token: &str,
        request: &AppendRunLogRequest,
        deadline: std::time::Instant,
    ) -> Result<AppendRunLogResponse, ClientError> {
        self.post_json_with_deadline(
            &["runs", run_id, "logs"],
            runner_token,
            request,
            Some(deadline),
        )
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        runner_token: &str,
        request: &FinishRunRequest,
    ) -> Result<FinishRunResponse, ClientError> {
        self.post_json(&["runs", run_id, "finish"], runner_token, request)
    }

    pub fn get_issue(
        &self,
        issue_id: &str,
        run_key: &str,
    ) -> Result<IssueDetailResponse, ClientError> {
        let url = self.endpoint(&["issues", issue_id])?;
        self.send_json(self.http.get(url), run_key)
    }

    fn post_json<T, B>(&self, path: &[&str], bearer_token: &str, body: &B) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.post_json_with_deadline(path, bearer_token, body, None)
    }

    fn post_json_with_deadline<T, B>(
        &self,
        path: &[&str],
        bearer_token: &str,
        body: &B,
        deadline: Option<std::time::Instant>,
    ) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let body = serde_json::to_vec(body).map_err(|_| ClientError::local_protocol_error())?;
        let url = self.endpoint(path)?;
        let mut request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::transport_error());
            }
            request = request.timeout(remaining.min(self.request_timeout));
        }
        self.send_json(request, bearer_token)
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        bearer_token: &str,
    ) -> Result<T, ClientError> {
        let response = self.send(request, bearer_token)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.json::<serde_json::Value>().unwrap_or_default();
            return Err(ClientError::http_response(status, &body));
        }
        response
            .json::<T>()
            .map_err(|_| ClientError::local_protocol_error())
    }

    fn send(
        &self,
        request: RequestBuilder,
        bearer_token: &str,
    ) -> Result<reqwest::blocking::Response, ClientError> {
        let authorization = HeaderValue::from_str(&format!("Bearer {bearer_token}"))
            .map_err(|_| ClientError::local_protocol_error())?;
        request
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, authorization)
            .send()
            .map_err(|_| ClientError::transport_error())
    }

    fn endpoint(&self, path: &[&str]) -> Result<Url, ClientError> {
        let mut url = self.base_url.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| ClientError::local_protocol_error())?;
        segments.pop_if_empty().push("api").push("v1");
        for segment in path {
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }
}

const MAX_LOG_BATCH_BYTES: usize = 32 * 1024;
const LOG_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(60);
const LOG_RETRY_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Buffered run output with ordered, retry-safe delivery to Tines.
///
/// Output is batched at 32 KiB. Call [`flush`](Self::flush) on a timer to send
/// a smaller live batch, or [`flush_before_finish`](Self::flush_before_finish)
/// before an ordinary finish report. Every clone shares the same queue and
/// cancellation state.
#[derive(Clone)]
pub struct RunLogBuffer {
    shared: Arc<RunLogShared>,
}

struct RunLogShared {
    state: Mutex<RunLogState>,
    send_lock: Mutex<()>,
    stopped: AtomicBool,
    cancelled: AtomicBool,
}

struct RunLogState {
    preparation_output: String,
    output_buffer: String,
    pending_batches: VecDeque<String>,
    next_seq: u64,
    harness_started: bool,
    input_closed: bool,
    redactor: SecretRedactor,
}

impl RunLogBuffer {
    pub fn new() -> Self {
        Self::with_secrets(std::iter::empty::<String>())
    }

    /// Create a per-run log buffer that redacts the supplied secret values.
    pub fn with_secrets(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            shared: Arc::new(RunLogShared {
                state: Mutex::new(RunLogState {
                    preparation_output: String::new(),
                    output_buffer: String::new(),
                    pending_batches: VecDeque::new(),
                    next_seq: 1,
                    harness_started: false,
                    input_closed: false,
                    redactor: SecretRedactor::new(secrets),
                }),
                send_lock: Mutex::new(()),
                stopped: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
            }),
        }
    }

    /// Build a redacting buffer from the run key and secret assignment values.
    pub fn for_assignment(assignment: &crate::protocol::RunnerAssignment) -> Self {
        let secrets = std::iter::once(assignment.run_key.clone()).chain(
            assignment
                .env
                .iter()
                .filter(|entry| entry.secret)
                .map(|entry| entry.value.clone()),
        );
        Self::with_secrets(secrets)
    }

    /// Keep workspace output for delivery after the harness starts.
    pub fn buffer_preparation_output(&self, chunk: &str) {
        if chunk.is_empty() || self.shared.stopped.load(Ordering::Acquire) {
            return;
        }
        let _send_guard = lock_unpoisoned(&self.shared.send_lock);
        if self.shared.stopped.load(Ordering::Acquire) {
            return;
        }
        let mut state = lock_unpoisoned(&self.shared.state);
        if state.input_closed || state.harness_started {
            return;
        }
        let redacted = state.redactor.write(chunk);
        state.preparation_output.push_str(&redacted);
    }

    /// Return buffered preparation output for a failed-run diagnostic.
    pub fn preparation_output(&self) -> String {
        let state = lock_unpoisoned(&self.shared.state);
        let mut output = state.preparation_output.clone();
        if state.redactor.has_pending() {
            output.push_str("[REDACTED]");
        }
        output
    }

    /// Flush preparation output after a successful harness spawn. An empty
    /// append marks a quiet harness as running when preparation produced no
    /// visible output.
    pub fn harness_started(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        self.mark_harness_started(client, run_id, runner_token, None)
    }

    /// Start the harness and flush preparation logs before an absolute deadline.
    pub fn harness_started_until(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
        deadline: Instant,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        self.mark_harness_started(client, run_id, runner_token, Some(deadline))
    }

    fn mark_harness_started(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
        deadline: Option<Instant>,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        let _send_guard = lock_unpoisoned(&self.shared.send_lock);
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        {
            let mut state = lock_unpoisoned(&self.shared.state);
            if state.input_closed {
                return Ok(None);
            }
            if !state.harness_started {
                state.harness_started = true;
                let preparation = std::mem::take(&mut state.preparation_output);
                state.append_output(&preparation);
                if state.output_buffer.is_empty() && state.pending_batches.is_empty() {
                    state.pending_batches.push_back(String::new());
                }
            }
        }
        self.drain(client, run_id, runner_token, true, false, deadline)
    }

    /// Add harness output. Full batches are sent at once; smaller output stays
    /// buffered until the next timer flush, explicit flush, or finish.
    pub fn append_harness_output(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
        chunk: &str,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        if chunk.is_empty() || self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        let _send_guard = lock_unpoisoned(&self.shared.send_lock);
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        {
            let mut state = lock_unpoisoned(&self.shared.state);
            if state.input_closed {
                return Ok(None);
            }
            if !state.harness_started {
                state.harness_started = true;
                let preparation = std::mem::take(&mut state.preparation_output);
                state.append_output(&preparation);
            }
            let redacted = state.redactor.write(chunk);
            state.append_output(&redacted);
        }
        self.drain(client, run_id, runner_token, false, false, None)
    }

    /// Send a buffered partial batch while the harness is still running.
    pub fn flush(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        let _send_guard = lock_unpoisoned(&self.shared.send_lock);
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.drain(client, run_id, runner_token, true, false, None)
    }

    /// Flush all valid output and close the stream before an ordinary finish.
    /// Retryable failures keep the same head batch and sequence until Tines
    /// accepts it or a caller stops the stream after supervisor settlement.
    pub fn flush_before_finish(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        let _send_guard = lock_unpoisoned(&self.shared.send_lock);
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        {
            let mut state = lock_unpoisoned(&self.shared.state);
            state.input_closed = true;
            if !state.harness_started {
                state.harness_started = true;
                let preparation = std::mem::take(&mut state.preparation_output);
                state.append_output(&preparation);
            }
        }
        let response = self.drain(client, run_id, runner_token, true, true, None)?;
        self.shared.stopped.store(true, Ordering::Release);
        Ok(response)
    }

    /// Stop delivery after supervisor cancellation or settlement.
    pub fn stop_sending(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.stopped.store(true, Ordering::Release);
    }

    /// Whether this run's log stream has been stopped.
    pub fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::Acquire)
    }

    /// Whether Tines canceled or settled this run's log stream.
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }

    fn drain(
        &self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
        flush_partial: bool,
        finish_redactor: bool,
        deadline: Option<Instant>,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        if finish_redactor {
            let mut state = lock_unpoisoned(&self.shared.state);
            let tail = state.redactor.finish();
            state.append_output(&tail);
        }
        {
            let mut state = lock_unpoisoned(&self.shared.state);
            if flush_partial {
                state.queue_partial_batch();
            }
        }

        let mut last_response = None;
        let mut consecutive_failures = 0u32;
        loop {
            if self.shared.stopped.load(Ordering::Acquire) {
                return Ok(last_response);
            }
            let next = {
                let state = lock_unpoisoned(&self.shared.state);
                state
                    .pending_batches
                    .front()
                    .map(|chunk| (chunk.clone(), state.next_seq))
            };
            let Some((chunk, seq)) = next else {
                return Ok(last_response);
            };

            let request = AppendRunLogRequest {
                chunk: chunk.clone(),
                seq: Some(seq),
            };
            let append = match deadline {
                Some(deadline) => {
                    client.append_run_log_until(run_id, runner_token, &request, deadline)
                }
                None => client.append_run_log(run_id, runner_token, &request),
            };
            match append {
                Ok(response) => {
                    if response.log_seq < seq {
                        return Err(ClientError::local_protocol_error());
                    }
                    let next_seq = response
                        .log_seq
                        .checked_add(1)
                        .ok_or_else(ClientError::local_protocol_error)?;
                    let mut state = lock_unpoisoned(&self.shared.state);
                    let accepted = state.pending_batches.pop_front();
                    debug_assert_eq!(accepted.as_deref(), Some(chunk.as_str()));
                    state.next_seq = next_seq;
                    last_response = Some(response);
                    consecutive_failures = 0;
                }
                Err(error) if error.category() == ErrorCategory::Retryable => {
                    let remaining =
                        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    if remaining.is_some_and(|remaining| remaining.is_zero()) {
                        return Err(error);
                    }
                    let delay = log_retry_delay(consecutive_failures);
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(
                        run_id,
                        seq,
                        error = %error,
                        backoff_seconds = delay.as_secs_f64(),
                        "run-log append failed; retrying the same batch"
                    );
                    let delay = remaining.map_or(delay, |remaining| delay.min(remaining));
                    if !sleep_unless_stopped(delay, &self.shared.stopped) {
                        return Ok(last_response);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Default for RunLogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RunLogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock_unpoisoned(&self.shared.state);
        f.debug_struct("RunLogBuffer")
            .field("pending_preparation_bytes", &state.preparation_output.len())
            .field("pending_batches", &state.pending_batches.len())
            .field("pending_output_bytes", &state.output_buffer.len())
            .field("next_seq", &state.next_seq)
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

impl RunLogState {
    fn append_output(&mut self, mut text: &str) {
        while !text.is_empty() {
            let available = MAX_LOG_BATCH_BYTES.saturating_sub(self.output_buffer.len());
            let take = text
                .char_indices()
                .map(|(start, ch)| start + ch.len_utf8())
                .take_while(|end| *end <= available)
                .last()
                .unwrap_or(0);
            if take == 0 {
                self.queue_partial_batch();
                continue;
            }
            self.output_buffer.push_str(&text[..take]);
            text = &text[take..];
            if self.output_buffer.len() == MAX_LOG_BATCH_BYTES {
                self.queue_partial_batch();
            }
        }
    }

    fn queue_partial_batch(&mut self) {
        if !self.output_buffer.is_empty() {
            self.pending_batches
                .push_back(std::mem::take(&mut self.output_buffer));
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn log_retry_delay(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.min(6);
    Duration::from_secs(1u64 << exponent).min(LOG_RETRY_MAX_BACKOFF)
}

fn sleep_unless_stopped(duration: Duration, stopped: &AtomicBool) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        let interval = remaining.min(LOG_RETRY_CHECK_INTERVAL);
        thread::sleep(interval);
        remaining = remaining.saturating_sub(interval);
    }
    !stopped.load(Ordering::Acquire)
}

struct SecretRedactor {
    forms: Vec<String>,
    pending: String,
    finished: bool,
}

impl SecretRedactor {
    fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut forms = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .flat_map(|secret| {
                let json_escaped = serde_json::to_string(&secret)
                    .expect("a Rust string always serializes to JSON");
                [secret, json_escaped[1..json_escaped.len() - 1].to_owned()]
            })
            .collect::<Vec<_>>();
        forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
        forms.dedup();
        Self {
            forms,
            pending: String::new(),
            finished: false,
        }
    }

    fn write(&mut self, chunk: &str) -> String {
        if self.forms.is_empty() || self.finished {
            return redact_log_text(chunk, &self.forms);
        }
        let mut text = std::mem::take(&mut self.pending);
        text.push_str(chunk);
        let holdback = self.forms.iter().map(String::len).max().unwrap_or(1) - 1;
        let mut cut = text.len().saturating_sub(holdback);
        for form in &self.forms {
            if let Some(index) = text.find(form)
                && index < cut
                && index + form.len() > cut
            {
                cut = index;
            }
            for overlap in form.char_indices().map(|(index, _)| index).skip(1) {
                if overlap <= text.len() && text.ends_with(&form[..overlap]) {
                    cut = cut.min(text.len() - overlap);
                }
            }
        }
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        self.pending = text[cut..].to_owned();
        redact_log_text(&text[..cut], &self.forms)
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn finish(&mut self) -> String {
        self.finished = true;
        redact_log_text(&std::mem::take(&mut self.pending), &self.forms)
    }
}

fn redact_log_text(text: &str, forms: &[String]) -> String {
    let mut redacted = text.to_owned();
    for form in forms {
        redacted = redacted.replace(form, "***");
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::{Client, ErrorCategory, RunLogBuffer, classify_http_status};
    use crate::protocol::{
        AppendRunLogRequest, FinishRunRequest, FinishStatus, RegisterRunnerRequest, RunnerHarness,
        RunnerPollRequest,
    };
    use reqwest::StatusCode;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn read_log_request(stream: &mut TcpStream) -> serde_json::Value {
        let mut reader = BufReader::new(stream.try_clone().expect("clone request stream"));
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request header");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("read request body");
        serde_json::from_slice(&body).expect("parse log request")
    }

    fn write_log_response(stream: &mut TcpStream, seq: u64) {
        let body = format!("{{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":{seq}}}");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write log response");
    }

    #[test]
    fn classifies_http_failures() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert_eq!(classify_http_status(status), ErrorCategory::Authentication);
        }
        for status in [StatusCode::CONFLICT, StatusCode::LOCKED] {
            assert_eq!(classify_http_status(status), ErrorCategory::Fencing);
        }
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(classify_http_status(status), ErrorCategory::Retryable);
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::NOT_FOUND,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert_eq!(classify_http_status(status), ErrorCategory::Protocol);
        }
    }

    #[test]
    fn retries_the_server_policy_race_but_treats_daemon_conflict_as_fencing() {
        let retry = super::ClientError::http_response(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "error": {
                    "code": "runner_conflict",
                    "message": "runner policy changed during poll reconciliation; retry the poll"
                }
            }),
        );
        assert_eq!(retry.category(), ErrorCategory::Retryable);

        let fenced = super::ClientError::http_response(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "error": {
                    "code": "runner_conflict",
                    "message": "another daemon instance is serving this runner; this one has been superseded"
                }
            }),
        );
        assert_eq!(fenced.category(), ErrorCategory::Fencing);
    }

    #[test]
    fn classifies_transport_failure_as_retryable_without_echoing_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused port");
        let address = listener.local_addr().expect("read unused port");
        drop(listener);

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(1))
            .expect("create client");
        let error = client
            .get_issue("iss_1", "run-secret-token")
            .expect_err("closed port must fail");

        assert_eq!(error.category(), ErrorCategory::Retryable);
        assert_eq!(error.status(), None);
        assert!(!error.to_string().contains("run-secret-token"));
    }

    #[test]
    fn run_log_request_uses_the_remaining_deadline_as_its_http_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept log request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone request stream"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("read request body");

            thread::sleep(Duration::from_millis(600));
            let response = r#"{"status":"running","log_bytes_dropped":0,"log_seq":1}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let deadline = Instant::now() + Duration::from_millis(100);
        let started = Instant::now();
        let error = client
            .append_run_log_until(
                "arun_deadline",
                "runner-token",
                &AppendRunLogRequest {
                    chunk: String::new(),
                    seq: Some(1),
                },
                deadline,
            )
            .expect_err("server response must arrive after the deadline");

        assert_eq!(error.category(), ErrorCategory::Retryable);
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().expect("join fake Tines server");
    }

    #[test]
    fn preparation_logs_wait_for_the_first_harness_output() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept log request");
            let request = read_log_request(&mut stream);
            request_tx.send(request).expect("send captured request");
            write_log_response(&mut stream, 1);
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::new();
        logs.buffer_preparation_output("git [repo]: receiving objects\n");
        logs.buffer_preparation_output("git [repo]: checking out branch\n");

        assert!(request_rx.recv_timeout(Duration::from_millis(50)).is_err());

        assert!(
            logs.append_harness_output(&client, "arun_1", "runner-token", "codex started\n")
                .expect("buffer the first harness output")
                .is_none()
        );
        let response = logs
            .flush(&client, "arun_1", "runner-token")
            .expect("flush the first batch")
            .expect("receive log response");
        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 1);

        let request = request_rx.recv().expect("receive log request");
        assert_eq!(
            request["chunk"],
            "git [repo]: receiving objects\ngit [repo]: checking out branch\ncodex started\n"
        );
        assert_eq!(request["seq"], 1);
        server.join().expect("join test server");
    }

    #[test]
    fn harness_start_flushes_preparation_logs_without_waiting_for_output() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept log request");
            let request = read_log_request(&mut stream);
            request_tx.send(request).expect("send captured request");
            write_log_response(&mut stream, 1);
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::new();
        logs.buffer_preparation_output("git [repo]: receiving objects\n");
        logs.buffer_preparation_output("git [repo]: checking out branch\n");

        assert!(request_rx.recv_timeout(Duration::from_millis(50)).is_err());

        let response = logs
            .harness_started(&client, "arun_1", "runner-token")
            .expect("flush checkout logs when the harness starts")
            .expect("receive log response");
        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 1);

        let request = request_rx.recv().expect("receive log request");
        assert_eq!(
            request["chunk"],
            "git [repo]: receiving objects\ngit [repo]: checking out branch\n"
        );
        assert_eq!(request["seq"], 1);
        assert!(request_rx.recv_timeout(Duration::from_millis(50)).is_err());
        server.join().expect("join test server");
    }

    #[test]
    fn quiet_harness_start_sends_an_empty_start_marker_without_preparation_logs() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept start marker");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("read request body");
            let request: serde_json::Value =
                serde_json::from_slice(&body).expect("parse start marker");
            request_tx.send(request).expect("send captured request");

            let response_body = "{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":1}";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .expect("write log response");
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::new();
        let response = logs
            .harness_started(&client, "arun_1", "runner-token")
            .expect("mark quiet harness as started")
            .expect("receive log response");

        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 1);
        let request = request_rx.recv().expect("receive start marker");
        assert_eq!(request["chunk"], "");
        assert_eq!(request["seq"], 1);
        server.join().expect("join test server");
    }

    #[test]
    fn retries_batches_in_order_across_network_failures_without_duplicate_appends() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let server = thread::spawn(move || {
            let mut applied = Vec::new();
            let mut current_seq = 0;
            let mut requests = Vec::new();
            for attempt in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept log request");
                let request = read_log_request(&mut stream);
                let seq = request["seq"].as_u64().expect("request sequence");
                requests.push(request.clone());
                match attempt {
                    // Simulate Tines applying seq 1 before the response is lost.
                    0 => {
                        applied.push(request["chunk"].as_str().unwrap().to_owned());
                        current_seq = seq;
                    }
                    // A retry of an applied seq is acknowledged, but not appended again.
                    1 => {
                        if seq > current_seq {
                            applied.push(request["chunk"].as_str().unwrap().to_owned());
                            current_seq = seq;
                        }
                        write_log_response(&mut stream, current_seq);
                    }
                    // Simulate a network failure before seq 2 reaches Tines.
                    2 => {}
                    3 => {
                        if seq > current_seq {
                            applied.push(request["chunk"].as_str().unwrap().to_owned());
                            current_seq = seq;
                        }
                        write_log_response(&mut stream, current_seq);
                    }
                    _ => unreachable!(),
                }
            }
            (applied, requests)
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::new();
        let first = "a".repeat(super::MAX_LOG_BATCH_BYTES);
        let second = "b".repeat(super::MAX_LOG_BATCH_BYTES);
        let response = logs
            .append_harness_output(
                &client,
                "arun_1",
                "runner-token",
                &format!("{first}{second}"),
            )
            .expect("retry all full batches")
            .expect("receive final response");
        assert_eq!(response.log_seq, 2);

        let (applied, requests) = server.join().expect("join mock Tines server");
        assert_eq!(applied, [first, second]);
        assert_eq!(
            requests
                .iter()
                .map(|request| request["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 1, 2, 2]
        );
        assert_eq!(requests[0]["chunk"], requests[1]["chunk"]);
        assert_eq!(requests[2]["chunk"], requests[3]["chunk"]);
    }

    #[test]
    fn supervisor_stop_interrupts_retry_and_prevents_more_log_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept first log request");
            request_tx
                .send(read_log_request(&mut stream))
                .expect("send captured request");
            // Closing without a response produces a retryable transport failure.
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::new();
        let worker_logs = logs.clone();
        let worker = thread::spawn(move || {
            worker_logs.append_harness_output(
                &client,
                "arun_1",
                "runner-token",
                &"x".repeat(super::MAX_LOG_BATCH_BYTES),
            )
        });
        let first = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receive first attempt");
        assert_eq!(first["seq"], 1);
        server.join().expect("join test server");
        logs.stop_sending();
        assert!(
            worker
                .join()
                .expect("join log worker")
                .expect("stop retry cleanly")
                .is_none()
        );
        assert!(logs.is_stopped());
    }

    #[test]
    fn redacts_secret_values_across_output_chunks_and_flushes_before_finish() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for seq in 1..=2 {
                let (mut stream, _) = listener.accept().expect("accept log request");
                request_tx
                    .send(read_log_request(&mut stream))
                    .expect("send captured request");
                write_log_response(&mut stream, seq);
            }
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let logs = RunLogBuffer::with_secrets(["secret-token".to_owned()]);
        logs.buffer_preparation_output("launch begins with secret-");
        logs.buffer_preparation_output("token and safe details\n");
        logs.harness_started(&client, "arun_1", "runner-token")
            .expect("send the launch batch")
            .expect("receive launch response");
        assert!(
            logs.append_harness_output(&client, "arun_1", "runner-token", "last words")
                .expect("buffer the final partial output")
                .is_none()
        );
        logs.flush_before_finish(&client, "arun_1", "runner-token")
            .expect("flush before the ordinary finish report");
        assert!(logs.is_stopped());

        let requests: Vec<_> = (0..2)
            .map(|_| request_rx.recv().expect("receive log request"))
            .collect();
        let combined = format!(
            "{}{}",
            requests[0]["chunk"].as_str().unwrap(),
            requests[1]["chunk"].as_str().unwrap()
        );
        assert_eq!(requests[0]["seq"], 1);
        assert_eq!(requests[1]["seq"], 2);
        assert!(combined.contains("launch begins with *** and safe details\n"));
        assert!(combined.contains("last words"));
        assert!(!combined.contains("secret-token"));
        server.join().expect("join test server");
    }

    #[test]
    fn sends_endpoint_specific_bearer_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let server = thread::spawn(move || {
            let responses = [
                (
                    200,
                    r#"{"runner":{"id":"rnr_1"},"runner_token":"runner-secret"}"#,
                ),
                (
                    400,
                    r#"{"error":{"code":"invalid_json","message":"Request body must be valid JSON"}}"#,
                ),
                (200, r#"{"assignments":[],"cancels":[]}"#),
                (
                    200,
                    r#"{"status":"running","log_bytes_dropped":0,"log_seq":1}"#,
                ),
                (200, r#"{"id":"arun_1","status":"completed"}"#),
                (
                    200,
                    r#"{"id":"iss_1","workflow":{"name":"Implementation"}}"#,
                ),
            ];
            let mut requests = Vec::new();
            for (status, response_body) in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read request header");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().expect("content length");
                    }
                    request.push_str(&line);
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).expect("read request body");
                request.push_str(&String::from_utf8_lossy(&body));
                requests.push(request.to_ascii_lowercase());

                write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("write response");
            }
            requests
        });

        let client =
            Client::with_timeout(&format!("http://{address}/tines"), Duration::from_secs(5))
                .expect("create client");
        client
            .register_runner(
                "user-key",
                &RegisterRunnerRequest {
                    name: "workstation".into(),
                    harness: Some(RunnerHarness::Codex),
                    command: None,
                    max_concurrent: Some(1),
                    max_run_minutes: None,
                    hostname: None,
                    platform: None,
                },
            )
            .expect("register runner");
        client
            .verify_runner_token("rnr_1", "runner-token")
            .expect("verify runner token without polling");
        client
            .poll_runner(
                "rnr_1",
                "runner-token",
                &RunnerPollRequest {
                    instance_id: Some("boot-1".into()),
                    owned_runs: Vec::new(),
                    max_concurrent: None,
                    concurrency_control: None,
                    cancellation_acks: None,
                    declined_assignments: None,
                    draining: None,
                    env_delivery: Some(1),
                    effort_capabilities: None,
                },
            )
            .expect("poll runner");
        client
            .append_run_log(
                "arun_1",
                "runner-token",
                &AppendRunLogRequest {
                    chunk: "hello".into(),
                    seq: Some(1),
                },
            )
            .expect("append log");
        client
            .finish_run(
                "arun_1",
                "runner-token",
                &FinishRunRequest {
                    status: FinishStatus::Completed,
                    error: None,
                    provider_session_id: None,
                    usage: None,
                    pricing_evidence: None,
                    judgment: None,
                    resume_at: None,
                },
            )
            .expect("finish run");
        client.get_issue("iss_1", "run-key").expect("get issue");

        let requests = server.join().expect("join test server");
        assert!(requests[0].contains("post /tines/api/v1/runners/register "));
        assert!(requests[0].contains("authorization: bearer user-key"));
        assert!(requests[1].contains("post /tines/api/v1/runners/rnr_1/poll "));
        assert!(requests[1].contains("authorization: bearer runner-token"));
        assert!(requests[1].contains("content-length: 1\r\n"));
        assert!(requests[1].ends_with('{'));
        assert!(requests[2].contains("post /tines/api/v1/runners/rnr_1/poll "));
        assert!(requests[2].contains("authorization: bearer runner-token"));
        assert!(requests[3].contains("post /tines/api/v1/runs/arun_1/logs "));
        assert!(requests[3].contains("authorization: bearer runner-token"));
        assert!(requests[4].contains("post /tines/api/v1/runs/arun_1/finish "));
        assert!(requests[4].contains("authorization: bearer runner-token"));
        assert!(requests[5].contains("get /tines/api/v1/issues/iss_1 "));
        assert!(requests[5].contains("authorization: bearer run-key"));
    }
}
