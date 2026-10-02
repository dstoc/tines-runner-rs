//! Direct HTTP client for the Tines local-runner protocol.

use std::fmt;
use std::time::Duration;

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

        let http = HttpClient::builder()
            .timeout(timeout.min(MAX_REQUEST_TIMEOUT))
            // Credentials must remain scoped to the configured Tines host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClientError::local_protocol_error())?;

        Ok(Self { base_url, http })
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
        let body = serde_json::to_vec(body).map_err(|_| ClientError::local_protocol_error())?;
        let url = self.endpoint(path)?;
        let request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
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

/// Buffered workspace logs that are sent after the harness starts.
///
/// The first run-log append is also the server's harness-start signal. Keep
/// workspace preparation output here until the process launcher confirms that
/// the harness has started.
#[derive(Clone)]
pub struct RunLogBuffer {
    preparation_chunks: std::collections::VecDeque<String>,
    next_seq: u64,
}

impl RunLogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep workspace output for delivery with the first harness output.
    pub fn buffer_preparation_output(&mut self, chunk: &str) {
        if !chunk.is_empty() {
            self.preparation_chunks.push_back(chunk.to_owned());
        }
    }

    /// Return buffered preparation output for a failed-run diagnostic.
    pub fn preparation_output(&self) -> String {
        self.preparation_chunks.iter().cloned().collect()
    }

    /// Flush preparation logs after the process launcher confirms a successful
    /// harness start. An empty append marks a quiet harness as running when
    /// workspace preparation produced no output.
    pub fn harness_started(
        &mut self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
    ) -> Result<AppendRunLogResponse, ClientError> {
        if let Some(response) = self.flush_preparation_output(client, run_id, runner_token)? {
            return Ok(response);
        }

        let response = client.append_run_log(
            run_id,
            runner_token,
            &AppendRunLogRequest {
                chunk: String::new(),
                seq: Some(self.next_seq),
            },
        )?;
        self.next_seq = response.log_seq.saturating_add(1);
        Ok(response)
    }

    /// Append harness output, flushing any preparation output first.
    pub fn append_harness_output(
        &mut self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
        chunk: &str,
    ) -> Result<AppendRunLogResponse, ClientError> {
        self.flush_preparation_output(client, run_id, runner_token)?;

        let response = client.append_run_log(
            run_id,
            runner_token,
            &AppendRunLogRequest {
                chunk: chunk.to_owned(),
                seq: Some(self.next_seq),
            },
        )?;
        self.next_seq = response.log_seq.saturating_add(1);
        Ok(response)
    }

    fn flush_preparation_output(
        &mut self,
        client: &Client,
        run_id: &str,
        runner_token: &str,
    ) -> Result<Option<AppendRunLogResponse>, ClientError> {
        let mut last_response = None;
        while let Some(preparation_chunk) = self.preparation_chunks.front() {
            let response = client.append_run_log(
                run_id,
                runner_token,
                &AppendRunLogRequest {
                    chunk: preparation_chunk.clone(),
                    seq: Some(self.next_seq),
                },
            )?;
            self.preparation_chunks.pop_front();
            self.next_seq = response.log_seq.saturating_add(1);
            last_response = Some(response);
        }
        Ok(last_response)
    }
}

impl Default for RunLogBuffer {
    fn default() -> Self {
        Self {
            preparation_chunks: std::collections::VecDeque::new(),
            next_seq: 1,
        }
    }
}

impl fmt::Debug for RunLogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunLogBuffer")
            .field("pending_preparation_chunks", &self.preparation_chunks.len())
            .finish()
    }
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
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

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
    fn preparation_logs_wait_for_the_first_harness_output() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for seq in 1..=3 {
                let (mut stream, _) = listener.accept().expect("accept log request");
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
                    serde_json::from_slice(&body).expect("parse log request");
                request_tx.send(request).expect("send captured request");

                let response_body =
                    format!("{{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":{seq}}}");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("write log response");
            }
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let mut logs = RunLogBuffer::new();
        logs.buffer_preparation_output("git [repo]: receiving objects\n");
        logs.buffer_preparation_output("git [repo]: checking out branch\n");

        assert!(request_rx.recv_timeout(Duration::from_millis(50)).is_err());

        let response = logs
            .append_harness_output(&client, "arun_1", "runner-token", "codex started\n")
            .expect("append the first harness output");
        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 3);

        let requests: Vec<_> = (0..3)
            .map(|_| request_rx.recv().expect("receive log request"))
            .collect();
        assert_eq!(requests[0]["chunk"], "git [repo]: receiving objects\n");
        assert_eq!(requests[0]["seq"], 1);
        assert_eq!(requests[1]["chunk"], "git [repo]: checking out branch\n");
        assert_eq!(requests[1]["seq"], 2);
        assert_eq!(requests[2]["chunk"], "codex started\n");
        assert_eq!(requests[2]["seq"], 3);
        server.join().expect("join test server");
    }

    #[test]
    fn harness_start_flushes_preparation_logs_without_waiting_for_output() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for seq in 1..=2 {
                let (mut stream, _) = listener.accept().expect("accept log request");
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
                    serde_json::from_slice(&body).expect("parse log request");
                request_tx.send(request).expect("send captured request");

                let response_body =
                    format!("{{\"status\":\"running\",\"log_bytes_dropped\":0,\"log_seq\":{seq}}}");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("write log response");
            }
        });

        let client = Client::with_timeout(&format!("http://{address}"), Duration::from_secs(5))
            .expect("create client");
        let mut logs = RunLogBuffer::new();
        logs.buffer_preparation_output("git [repo]: receiving objects\n");
        logs.buffer_preparation_output("git [repo]: checking out branch\n");

        assert!(request_rx.recv_timeout(Duration::from_millis(50)).is_err());

        let response = logs
            .harness_started(&client, "arun_1", "runner-token")
            .expect("flush checkout logs when the harness starts");
        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 2);

        let requests: Vec<_> = (0..2)
            .map(|_| request_rx.recv().expect("receive log request"))
            .collect();
        assert_eq!(requests[0]["chunk"], "git [repo]: receiving objects\n");
        assert_eq!(requests[0]["seq"], 1);
        assert_eq!(requests[1]["chunk"], "git [repo]: checking out branch\n");
        assert_eq!(requests[1]["seq"], 2);
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
        let mut logs = RunLogBuffer::new();
        let response = logs
            .harness_started(&client, "arun_1", "runner-token")
            .expect("mark quiet harness as started");

        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 1);
        let request = request_rx.recv().expect("receive start marker");
        assert_eq!(request["chunk"], "");
        assert_eq!(request["seq"], 1);
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
