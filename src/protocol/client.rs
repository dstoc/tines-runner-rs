//! Direct HTTP client for the Tines local-runner protocol.

use std::fmt;
use std::time::Duration;

use reqwest::blocking::{Client as HttpClient, RequestBuilder};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::protocol::{
    AppendRunLogRequest, AppendRunLogResponse, FinishRunRequest, FinishRunResponse,
    IssueDetailResponse, RegisterRunnerRequest, RunnerIdentityResponse, RunnerPollRequest,
    RunnerPollResponse, RunnerTokenResponse,
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

    pub fn get_runner_identity(
        &self,
        runner_id: &str,
        runner_token: &str,
    ) -> Result<RunnerIdentityResponse, ClientError> {
        let url = self.endpoint(&["runners", runner_id, "identity"])?;
        self.send_json(self.http.get(url), runner_token)
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
        let authorization = HeaderValue::from_str(&format!("Bearer {bearer_token}"))
            .map_err(|_| ClientError::local_protocol_error())?;
        let response = request
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, authorization)
            .send()
            .map_err(|_| ClientError::transport_error())?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::http_status(status));
        }
        response
            .json::<T>()
            .map_err(|_| ClientError::local_protocol_error())
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

#[cfg(test)]
mod tests {
    use super::{Client, ErrorCategory, classify_http_status};
    use crate::protocol::{
        AppendRunLogRequest, FinishRunRequest, FinishStatus, RegisterRunnerRequest, RunnerHarness,
        RunnerPollRequest,
    };
    use reqwest::StatusCode;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
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
    fn sends_endpoint_specific_bearer_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("read listener address");
        let server = thread::spawn(move || {
            let responses = [
                r#"{"runner":{"id":"rnr_1"},"runner_token":"runner-secret"}"#,
                r#"{"runner_id":"rnr_1"}"#,
                r#"{"assignments":[],"cancels":[]}"#,
                r#"{"status":"running","log_bytes_dropped":0,"log_seq":1}"#,
                r#"{"id":"arun_1","status":"completed"}"#,
                r#"{"id":"iss_1","workflow":{"name":"Implementation"}}"#,
            ];
            let mut requests = Vec::new();
            for response_body in responses {
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
            .get_runner_identity("rnr_1", "runner-token")
            .expect("get runner identity");
        client
            .poll_runner(
                "rnr_1",
                "runner-token",
                &RunnerPollRequest {
                    instance_id: Some("boot-1".into()),
                    owned_runs: Vec::new(),
                    max_concurrent: None,
                    draining: None,
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
                },
            )
            .expect("finish run");
        client.get_issue("iss_1", "run-key").expect("get issue");

        let requests = server.join().expect("join test server");
        assert!(requests[0].contains("post /tines/api/v1/runners/register "));
        assert!(requests[0].contains("authorization: bearer user-key"));
        assert!(requests[1].contains("get /tines/api/v1/runners/rnr_1/identity "));
        assert!(requests[1].contains("authorization: bearer runner-token"));
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
