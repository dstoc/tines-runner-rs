//! A local, scripted implementation of the runner protocol used by integration tests.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RUNNER_ID: &str = "rnr_fake_tines";

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

#[derive(Default)]
struct TransientFailures {
    polls: usize,
    logs: usize,
    finishes: usize,
}

struct RoutedAssignment {
    runner_name: String,
    assignment: Value,
}

#[derive(Default)]
struct State {
    requests: Vec<RecordedRequest>,
    registered_runner_name: Option<String>,
    routed_assignments: VecDeque<RoutedAssignment>,
    unexpected_requests: Vec<String>,
    poll_responses: VecDeque<(u16, Value)>,
    failures: TransientFailures,
    accepted_logs: Vec<Value>,
    accepted_finishes: Vec<Value>,
}

pub struct FakeTines {
    address: SocketAddr,
    state: Arc<(Mutex<State>, Condvar)>,
    stopping: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl FakeTines {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Tines server");
        listener
            .set_nonblocking(true)
            .expect("make fake Tines listener nonblocking");
        let address = listener.local_addr().expect("read fake Tines address");
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let server_state = Arc::clone(&state);
        let server_stopping = Arc::clone(&stopping);
        let server = thread::spawn(move || serve(listener, server_state, server_stopping));

        Self {
            address,
            state,
            stopping,
            server: Some(server),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn enqueue_poll(&self, response: Value) {
        self.enqueue_poll_response(200, response);
    }

    pub fn enqueue_poll_response(&self, status: u16, response: Value) {
        let (lock, changed) = &*self.state;
        lock_state(lock)
            .poll_responses
            .push_back((status, response));
        changed.notify_all();
    }

    /// Route one issue assignment to a registered runner by its configured name.
    pub fn route_issue(&self, runner_name: impl Into<String>, assignment: Value) {
        let (lock, changed) = &*self.state;
        lock_state(lock)
            .routed_assignments
            .push_back(RoutedAssignment {
                runner_name: runner_name.into(),
                assignment,
            });
        changed.notify_all();
    }

    pub fn fail_next_polls(&self, count: usize) {
        lock_state(&self.state.0).failures.polls = count;
    }

    pub fn fail_next_logs(&self, count: usize) {
        lock_state(&self.state.0).failures.logs = count;
    }

    pub fn fail_next_finishes(&self, count: usize) {
        lock_state(&self.state.0).failures.finishes = count;
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock_state(&self.state.0).requests.clone()
    }

    pub fn accepted_logs(&self) -> Vec<Value> {
        lock_state(&self.state.0).accepted_logs.clone()
    }

    pub fn accepted_finishes(&self) -> Vec<Value> {
        lock_state(&self.state.0).accepted_finishes.clone()
    }

    pub fn unexpected_requests(&self) -> Vec<String> {
        lock_state(&self.state.0).unexpected_requests.clone()
    }

    pub fn wait_for(
        &self,
        timeout: Duration,
        predicate: impl Fn(&[RecordedRequest]) -> bool,
    ) -> Vec<RecordedRequest> {
        let deadline = Instant::now() + timeout;
        let (lock, changed) = &*self.state;
        let mut state = lock_state(lock);
        loop {
            if predicate(&state.requests) {
                return state.requests.clone();
            }
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for fake Tines request");
            let (next, _) = changed
                .wait_timeout(state, (deadline - now).min(Duration::from_millis(50)))
                .expect("wait for fake Tines request");
            state = next;
        }
    }

    pub fn wait_for_finishes(&self, count: usize, timeout: Duration) -> Vec<Value> {
        self.wait_for(timeout, |requests| {
            requests
                .iter()
                .filter(|request| request.target.ends_with("/finish"))
                .count()
                >= count
        });
        let deadline = Instant::now() + timeout;
        loop {
            let finishes = self.accepted_finishes();
            if finishes.len() >= count {
                return finishes;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for accepted finish"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for FakeTines {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn serve(listener: TcpListener, state: Arc<(Mutex<State>, Condvar)>, stopping: Arc<AtomicBool>) {
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let request = match read_request(&mut stream) {
                    Ok(request) => request,
                    Err(_) => continue,
                };
                let (lock, changed) = &*state;
                let mut state = lock_state(lock);
                state.requests.push(request.clone());
                changed.notify_all();
                let response = response_for(&request, &mut state);
                drop(state);
                let _ = write_response(&mut stream, response);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

struct Response {
    status: u16,
    body: Value,
}

fn response_for(request: &RecordedRequest, state: &mut State) -> Response {
    if request.method == "POST" && request.target == "/api/v1/runners/register" {
        state.registered_runner_name = request.json()["name"].as_str().map(str::to_owned);
        return response(
            201,
            json!({
                "runner": {"id": RUNNER_ID},
                "runner_token": "fake-runner-token"
            }),
        );
    }

    if request.method == "POST" && request.target == format!("/api/v1/runners/{RUNNER_ID}/poll") {
        if request.body == b"{" {
            return response(
                400,
                json!({"error": {"code": "invalid_json", "message": "invalid JSON"}}),
            );
        }
        if state.failures.polls > 0 {
            state.failures.polls -= 1;
            return response(503, json!({"error": {"code": "unavailable"}}));
        }
        let registered_runner_name = state.registered_runner_name.as_deref();
        if let Some(index) = state
            .routed_assignments
            .iter()
            .position(|routed| registered_runner_name == Some(routed.runner_name.as_str()))
        {
            let routed = state
                .routed_assignments
                .remove(index)
                .expect("matching routed assignment exists");
            return response(
                200,
                json!({"assignments": [routed.assignment], "cancels": []}),
            );
        }
        let (status, mut body) = state
            .poll_responses
            .pop_front()
            .unwrap_or_else(|| (200, json!({"assignments": [], "cancels": []})));
        if status == 200 {
            let poll_request = serde_json::from_slice::<Value>(&request.body).unwrap_or_default();
            let body_object = body
                .as_object_mut()
                .expect("fake poll responses must be JSON objects");
            for (request_key, response_key) in [
                ("cancellation_acks", "cancellation_acks"),
                ("declined_assignments", "released_assignments"),
            ] {
                if body_object.get(response_key).is_none()
                    && let Some(value) = poll_request.get(request_key)
                {
                    body_object.insert(response_key.to_owned(), value.clone());
                }
            }
        }
        return response(status, body);
    }

    if request.method == "GET" && request.target.starts_with("/api/v1/issues/") {
        let issue_id = request.target.trim_start_matches("/api/v1/issues/");
        let expected_token = issue_id
            .strip_prefix("iss_")
            .map(|run_id| format!("Bearer issue-run-key-{run_id}"));
        if request.header("authorization") != expected_token.as_deref() {
            return response(
                401,
                json!({"error": {"code": "run_key_inactive", "message": "invalid issue run key"}}),
            );
        }
        return response(
            200,
            json!({"id": issue_id, "workflow": {"name": "Implementation"}}),
        );
    }

    if request.method == "POST" && request.target.starts_with("/api/v1/runs/") {
        let rest = request.target.trim_start_matches("/api/v1/runs/");
        if let Some(run_id) = rest.strip_suffix("/logs") {
            if state.failures.logs > 0 {
                state.failures.logs -= 1;
                return response(503, json!({"error": {"code": "unavailable"}}));
            }
            let log = serde_json::from_slice::<Value>(&request.body)
                .expect("fake run log request is JSON");
            let seq = log["seq"].as_u64().expect("run log has a sequence");
            state.accepted_logs.push(log);
            return response(
                200,
                json!({"status": "running", "log_bytes_dropped": 0, "log_seq": seq, "run_id": run_id}),
            );
        }
        if let Some(run_id) = rest.strip_suffix("/finish") {
            if state.failures.finishes > 0 {
                state.failures.finishes -= 1;
                return response(503, json!({"error": {"code": "unavailable"}}));
            }
            let finish = serde_json::from_slice::<Value>(&request.body)
                .expect("fake finish request is JSON");
            state.accepted_finishes.push(finish.clone());
            return response(200, json!({"id": run_id, "status": finish["status"]}));
        }
    }

    state
        .unexpected_requests
        .push(format!("{} {}", request.method, request.target));
    response(
        404,
        json!({"error": {"code": "unexpected_request", "target": request.target}}),
    )
}

fn response(status: u16, body: Value) -> Response {
    Response { status, body }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<RecordedRequest> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed before request headers completed",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut lines = header_text.lines();
    let mut request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "empty request"))?
        .split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed before request body completed",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(RecordedRequest {
        method,
        target,
        headers,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn write_response(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let body = serde_json::to_vec(&response.body).expect("encode fake Tines response");
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Fake Response",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
