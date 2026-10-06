#![cfg(unix)]

mod support {
    pub mod fake_tines;
}

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_tines::{FakeTines, RecordedRequest};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tines-runner-fake-tines-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create integration test directory");
        Self { path }
    }

    fn create_stub(&self) -> std::path::PathBuf {
        let path = self.path.join("stub-executor");
        fs::write(&path, include_str!("support/stub_executor.sh")).expect("write executor stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make executor stub executable");
        self.install_codex_stub();
        path
    }

    fn install_codex_stub(&self) {
        let bin = self.path.join("bin");
        fs::create_dir_all(&bin).expect("create isolated Codex PATH directory");
        let codex = bin.join("codex");
        fs::write(&codex, include_str!("support/stub_codex.sh")).expect("install fake Codex CLI");
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
            .expect("make fake Codex CLI executable");
    }

    fn write_events(&self, run_id: &str, events: impl IntoIterator<Item = Value>) {
        let directory = self.path.join("events");
        fs::create_dir_all(&directory).expect("create executor event fixture directory");
        let mut contents = String::new();
        for event in events {
            contents.push_str(&serde_json::to_string(&event).expect("encode executor event"));
            contents.push('\n');
        }
        fs::write(directory.join(format!("{run_id}.jsonl")), contents)
            .expect("write executor event fixture");
    }

    fn configure(
        &self,
        server_url: &str,
        stub: &std::path::Path,
        max_concurrent: usize,
        selected_override: bool,
    ) {
        let config_dir = self.path.join("config/tines-runner-rs");
        fs::create_dir_all(&config_dir).expect("create runner config directory");
        let credentials = self.path.join("credentials.toml");
        let state_dir = self.path.join("state");
        let workspaces = self.path.join("workspaces");
        let events = self.path.join("events");
        let captures = self.path.join("captures");
        let control = self.path.join("control");
        for directory in [&events, &captures, &control] {
            fs::create_dir_all(directory).expect("create executor fixture directory");
        }
        let default_executor = executor(stub, "default", &events, &captures, &control);
        let override_section = if selected_override {
            format!(
                "\n[[override]]\nproject = \"Tines\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nexecutor = {}\n",
                executor(stub, "selected", &events, &captures, &control)
            )
        } else {
            String::new()
        };
        let config = format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"fake-tines-integration\"\nexecutor_cwd = \"~\"\nexecutor = {default_executor}\nworkspace_parent = {:?}\nmax_concurrent = {max_concurrent}\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {:?}\nstate_dir = {:?}\n{override_section}",
            workspaces, credentials, state_dir
        );
        fs::write(config_dir.join("config.toml"), config).expect("write runner config");
    }

    fn configure_native(&self, server_url: &str, max_concurrent: usize) {
        let config_dir = self.path.join("config/tines-runner-rs");
        fs::create_dir_all(&config_dir).expect("create runner config directory");
        let credentials = self.path.join("credentials.toml");
        let state_dir = self.path.join("state");
        let workspaces = self.path.join("workspaces");
        fs::create_dir_all(&workspaces).expect("create native workspace parent");
        let config = format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"fake-tines-integration\"\nexecutor_cwd = \"~\"\nworkspace_parent = {:?}\nmax_concurrent = {max_concurrent}\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {:?}\nstate_dir = {:?}\n",
            workspaces, credentials, state_dir
        );
        fs::write(config_dir.join("config.toml"), config).expect("write native runner config");
    }

    fn credentials_path(&self) -> std::path::PathBuf {
        self.path.join("credentials.toml")
    }

    fn active_runs_path(&self) -> std::path::PathBuf {
        self.path.join("state/runner-default/active-runs.json")
    }

    fn workspace_parent(&self) -> std::path::PathBuf {
        self.path.join("workspaces")
    }

    fn runner(&self, bootstrap_key: Option<&str>) -> RunnerProcess {
        self.runner_for(
            &self.path.join("config/tines-runner-rs/config.toml"),
            None,
            bootstrap_key,
        )
    }

    fn runner_for(
        &self,
        config_path: &std::path::Path,
        runner_id: Option<&str>,
        bootstrap_key: Option<&str>,
    ) -> RunnerProcess {
        let mut command = self.runner_command(config_path, runner_id, bootstrap_key);
        RunnerProcess {
            child: command.spawn().expect("start runner daemon"),
        }
    }

    fn runner_command(
        &self,
        config_path: &std::path::Path,
        runner_id: Option<&str>,
        bootstrap_key: Option<&str>,
    ) -> Command {
        let config_home = self.path.join("config");
        let data_home = self.path.join("data");
        let runner_binary_directory = std::path::Path::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
            .parent()
            .expect("runner binary has a parent directory");
        let mut search_path = vec![self.path.join("bin"), runner_binary_directory.to_owned()];
        search_path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let search_path = std::env::join_paths(search_path).expect("build isolated PATH");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
        command.arg("--config").arg(config_path);
        if let Some(runner_id) = runner_id {
            command.args(["--runner", runner_id]);
        }
        command
            .env("HOME", &self.path)
            .env("USERPROFILE", &self.path)
            .env("XDG_CONFIG_HOME", config_home)
            .env("XDG_DATA_HOME", data_home)
            .env("APPDATA", &self.path)
            .env("LOCALAPPDATA", &self.path)
            .env("PATH", search_path)
            .env("RUST_LOG", "error")
            .env_remove("TINES_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(key) = bootstrap_key {
            command.env("TINES_API_KEY", key);
        }
        command
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct RunnerProcess {
    child: Child,
}

impl RunnerProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .args([format!("-{signal}"), self.id().to_string()])
            .status()
            .expect("send signal to runner");
        assert!(status.success(), "could not send {signal} to runner");
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll runner process") {
                return status;
            }
            assert!(Instant::now() < deadline, "runner process did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_with_output(mut self, timeout: Duration) -> std::process::Output {
        let status = self.wait(timeout);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_end(&mut stdout).expect("read runner stdout");
        }
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_end(&mut stderr).expect("read runner stderr");
        }
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    }

    fn assert_running(&mut self) {
        assert!(
            self.child
                .try_wait()
                .expect("poll runner process")
                .is_none(),
            "runner process exited unexpectedly"
        );
    }

    fn kill_now(&mut self) -> ExitStatus {
        self.signal("KILL");
        self.child.wait().expect("wait for killed runner")
    }
}

impl Drop for RunnerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

fn executor(
    stub: &std::path::Path,
    label: &str,
    events: &std::path::Path,
    captures: &std::path::Path,
    control: &std::path::Path,
) -> String {
    serde_json::to_string(&vec![
        stub.to_string_lossy().into_owned(),
        label.to_owned(),
        events.to_string_lossy().into_owned(),
        captures.to_string_lossy().into_owned(),
        control.to_string_lossy().into_owned(),
    ])
    .expect("encode executor argv")
}

fn assignment(run_id: &str, timeout_minutes: u64, env: Vec<Value>) -> Value {
    json!({
        "run": {
            "id": run_id,
            "issue_id": format!("iss_{run_id}"),
            "issue_ref": {
                "project_name": "Tines",
                "number": 22,
                "title": "Fake Tines integration"
            },
            "state_at_start_name": "Implement",
            "model": "gpt-5.1-codex"
        },
        "prompt": format!("exercise {run_id}"),
        "bundle": {"skills": [], "repos": []},
        "run_key": format!("issue-run-key-{run_id}"),
        "timeout_minutes": timeout_minutes,
        "env": env
    })
}

fn create_local_repository(directory: &std::path::Path) -> std::path::PathBuf {
    fs::create_dir_all(directory).expect("create local acceptance repository");
    fs::write(
        directory.join("acceptance.txt"),
        "materialized from routed test repository\n",
    )
    .expect("write acceptance repository fixture");
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.name", "Acceptance Test"][..],
        &["config", "user.email", "acceptance@example.invalid"][..],
        &["add", "acceptance.txt"][..],
        &["commit", "--quiet", "-m", "acceptance fixture"][..],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .output()
            .expect("run git to prepare acceptance repository");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    directory.to_owned()
}

fn poll_requests(requests: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    requests
        .iter()
        .filter(|request| request.target.ends_with("/poll"))
        .collect()
}

fn wait_for_quiet_poll(fake: &FakeTines, run_id: &str) {
    fake.wait_for(Duration::from_secs(10), |requests| {
        let finish_index = requests
            .iter()
            .position(|request| request.target == format!("/api/v1/runs/{run_id}/finish"));
        finish_index.is_some_and(|finish_index| {
            requests.iter().enumerate().any(|(index, request)| {
                index > finish_index
                    && request.target.ends_with("/poll")
                    && request.json()["owned_runs"] == json!([])
            })
        })
    });
}

fn wait_for_run_log(fake: &FakeTines, run_id: &str, message: &str) {
    let target = format!("/runs/{run_id}/logs");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let requests = fake.requests();
        if requests.iter().any(|request| {
            request.target.ends_with(&target)
                && request.json()["chunk"]
                    .as_str()
                    .is_some_and(|chunk| chunk.contains(message))
        }) {
            return;
        }
        let log_requests = requests
            .iter()
            .filter(|request| request.target.ends_with(&target))
            .map(RecordedRequest::json)
            .collect::<Vec<_>>();
        assert!(
            Instant::now() < deadline,
            "timed out waiting for run log {message:?}; received run logs {log_requests:?} and requests {:?}",
            requests
                .iter()
                .map(|request| &request.target)
                .collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn stop_gracefully(fake: &FakeTines, runner: &mut RunnerProcess) {
    runner.signal("TERM");
    let status = runner.wait(Duration::from_secs(10));
    assert!(status.success(), "graceful shutdown failed: {status}");
    fake.wait_for(Duration::from_secs(3), |requests| {
        poll_requests(requests)
            .iter()
            .any(|request| request.json()["draining"] == true)
    });
}

#[test]
fn default_native_executor_preserves_cold_workspace_environment_logs_and_finish() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    directory.install_codex_stub();
    directory.configure_native(fake.url().as_str(), 1);
    let local_repository = create_local_repository(&directory.path.join("native-repository"));
    let workspace_probe = directory.path.join("native-workspace.txt");
    let environment_probe = directory.path.join("native-environment.txt");
    let run_key_probe = directory.path.join("native-run-key.json");
    let mut native_assignment = assignment(
        "arun_happy",
        5,
        vec![
            json!({"name":"FAKE_CODEX_WORKSPACE_PROBE_FILE","value":workspace_probe,"secret":false}),
            json!({"name":"FAKE_CODEX_ENV_PROBE_FILE","value":environment_probe,"secret":false}),
            json!({"name":"FAKE_CODEX_RUN_KEY_PROBE_FILE","value":run_key_probe,"secret":false}),
            json!({"name":"DEPLOY_TOKEN","value":"native-integration-secret","secret":true}),
        ],
    );
    native_assignment["bundle"]["repos"] = json!([{
        "url": local_repository.to_string_lossy(),
        "branch": null,
        "dir": "materialized"
    }]);
    fake.route_issue("fake-tines-integration", native_assignment);

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(30));
    let finish = &finishes[0];
    assert_eq!(
        finish["status"],
        "completed",
        "{finish}; logs: {:?}",
        fake.accepted_logs()
    );
    assert_eq!(finish["provider_session_id"], "stub-thread");
    assert_eq!(finish["usage"]["input_tokens"], 75);
    assert_eq!(finish["usage"]["cache_read_tokens"], 20);
    assert_eq!(finish["usage"]["cache_write_tokens"], 5);
    assert_eq!(finish["usage"]["output_tokens"], 7);

    let workspace = fs::read_to_string(workspace_probe).expect("native Codex workspace probe");
    assert!(workspace.contains("prompt=exercise arun_happy"));
    assert!(workspace.contains("repo=materialized from routed test repository"));
    assert!(workspace.contains("/workspaces/"));
    assert_eq!(
        fs::read_to_string(environment_probe)
            .expect("assignment environment probe")
            .trim(),
        "native-integration-secret"
    );
    assert!(
        fs::read_to_string(run_key_probe)
            .expect("run-key API probe")
            .contains("iss_arun_happy")
    );

    let logs = fake
        .accepted_logs()
        .into_iter()
        .filter_map(|log| log["chunk"].as_str().map(str::to_owned))
        .collect::<String>();
    assert!(logs.contains("[session] started (thread stub-thread)"));
    assert!(logs.contains("[session] turn completed"));
    assert!(!logs.contains("native-integration-secret"));
    let issue_requests = fake
        .requests()
        .into_iter()
        .filter(|request| request.target == "/api/v1/issues/iss_arun_happy")
        .collect::<Vec<_>>();
    assert!(issue_requests.iter().any(|request| {
        request.header("authorization") == Some("Bearer issue-run-key-arun_happy")
    }));
    assert!(
        issue_requests.len() >= 2,
        "Codex used the run key inside the executor"
    );
    wait_for_quiet_poll(&fake, "arun_happy");
    assert_eq!(
        fs::read_dir(directory.workspace_parent()).unwrap().count(),
        0
    );
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn read_only_external_credentials_run_with_state_in_the_configured_directory() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);

    let mut bootstrap = directory.runner(Some("fake-bootstrap-key"));
    fake.wait_for(Duration::from_secs(10), |requests| {
        !poll_requests(requests).is_empty()
    });
    stop_gracefully(&fake, &mut bootstrap);

    let credentials_dir = directory.path.join("external-credentials");
    fs::create_dir_all(&credentials_dir).expect("create external credentials directory");
    let external_credentials = credentials_dir.join("runner-credentials");
    fs::copy(directory.credentials_path(), &external_credentials)
        .expect("install externally managed credentials");
    fs::set_permissions(&external_credentials, fs::Permissions::from_mode(0o400))
        .expect("make external credentials read-only");
    fs::set_permissions(&credentials_dir, fs::Permissions::from_mode(0o500))
        .expect("make external credentials directory read-only");

    let config_path = directory.path.join("config/tines-runner-rs/config.toml");
    let old_config = fs::read_to_string(&config_path).expect("read runner config");
    let old_credentials = format!("{:?}", directory.credentials_path());
    let new_credentials = format!("{external_credentials:?}");
    let config = old_config.replace(&old_credentials, &new_credentials);
    assert_ne!(config, old_config, "replace configured credentials path");
    fs::write(&config_path, config).expect("write config for external credentials");

    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_read_only_credentials", 5, Vec::new())],
        "cancels": []
    }));
    let mut runner = directory.runner(None);
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(20));
    assert_eq!(finishes[0]["status"], "completed");
    assert!(directory.active_runs_path().is_file());
    assert!(
        !credentials_dir.join("active-runs.json").exists(),
        "the read-only credential directory receives no daemon state"
    );
    assert!(fs::read(&external_credentials).is_ok());
    stop_gracefully(&fake, &mut runner);
    fs::set_permissions(&credentials_dir, fs::Permissions::from_mode(0o700))
        .expect("restore external credentials directory permissions");
    fs::set_permissions(&external_credentials, fs::Permissions::from_mode(0o600))
        .expect("restore external credentials permissions");
}

#[test]
fn native_executor_maps_codex_rate_limits_and_enforces_assignment_timeout() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    directory.install_codex_stub();
    directory.configure_native(fake.url().as_str(), 2);
    let rate_limit_events = directory.path.join("native-rate-limit.jsonl");
    fs::write(
        &rate_limit_events,
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"native-limited\"}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":80,\"cached_input_tokens\":10,\"cache_write_input_tokens\":5,\"output_tokens\":9}}\n",
            "{\"type\":\"turn.failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit exceeded.\",\"resets_at\":2000000000}}\n"
        ),
    )
    .expect("write Codex rate-limit fixture");
    fake.enqueue_poll(json!({
        "assignments": [
            assignment("arun_native_rate_limit", 5, vec![json!({
                "name":"FAKE_CODEX_JSONL_FILE",
                "value":rate_limit_events,
                "secret":false
            })]),
            assignment("arun_native_timeout", 0, vec![json!({
                "name":"FAKE_CODEX_SLEEP_SECONDS",
                "value":"30",
                "secret":false
            })])
        ],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    fake.wait_for_finishes(2, Duration::from_secs(30));
    let requests = fake.requests();
    let rate_finish = requests
        .iter()
        .find(|request| request.target == "/api/v1/runs/arun_native_rate_limit/finish")
        .expect("rate-limit finish request")
        .json();
    assert_eq!(rate_finish["status"], "failed");
    assert_eq!(rate_finish["judgment"], "rate_limited");
    assert_eq!(rate_finish["resume_at"], 2_000_000_000_000_u64);
    assert_eq!(rate_finish["provider_session_id"], "native-limited");
    assert_eq!(rate_finish["usage"]["input_tokens"], 65);
    assert_eq!(rate_finish["usage"]["output_tokens"], 9);

    let timeout_finish = requests
        .iter()
        .find(|request| request.target == "/api/v1/runs/arun_native_timeout/finish")
        .expect("timeout finish request")
        .json();
    assert_eq!(timeout_finish["status"], "failed");
    assert!(
        timeout_finish["error"]
            .as_str()
            .expect("timeout diagnostic")
            .contains("0-minute run timeout"),
        "timeout finish: {timeout_finish}"
    );
    wait_for_quiet_poll(&fake, "arun_native_timeout");
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn multiple_default_native_executors_run_concurrently_within_the_configured_limit() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    directory.install_codex_stub();
    directory.configure_native(fake.url().as_str(), 2);
    let barrier = directory.path.join("native-codex-barrier");
    fs::create_dir_all(&barrier).expect("create native Codex concurrency barrier");
    let mut assignments = ["arun_native_parallel_a", "arun_native_parallel_b"]
        .into_iter()
        .map(|run_id| {
            assignment(
                run_id,
                5,
                vec![
                    json!({"name":"FAKE_CODEX_BARRIER_DIR","value":barrier,"secret":false}),
                    json!({"name":"FAKE_CODEX_BARRIER_NAME","value":run_id,"secret":false}),
                    json!({"name":"FAKE_CODEX_BARRIER_COUNT","value":"2","secret":false}),
                ],
            )
        })
        .collect::<Vec<_>>();
    let declined_probe = directory.path.join("native-over-capacity-ran.txt");
    assignments.push(assignment(
        "arun_native_over_capacity",
        5,
        vec![json!({
            "name":"FAKE_CODEX_ENV_PROBE_FILE",
            "value":declined_probe,
            "secret":false
        })],
    ));
    fake.enqueue_poll(json!({"assignments": assignments, "cancels": []}));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(2, Duration::from_secs(30));
    assert_eq!(finishes.len(), 2);
    assert!(
        finishes
            .iter()
            .all(|finish| finish["status"] == "completed"),
        "native executor finishes: {finishes:?}"
    );
    assert!(barrier.join("arun_native_parallel_a").exists());
    assert!(barrier.join("arun_native_parallel_b").exists());
    fake.wait_for(Duration::from_secs(10), |requests| {
        poll_requests(requests).iter().any(|request| {
            request.json()["declined_assignments"]
                .as_array()
                .is_some_and(|declined| declined.contains(&json!("arun_native_over_capacity")))
        })
    });
    assert!(
        !declined_probe.exists(),
        "the daemon must not start an executor beyond configured concurrency"
    );
    assert_eq!(
        poll_requests(&fake.requests())[0].json()["max_concurrent"],
        2
    );
    wait_for_quiet_poll(&fake, "arun_native_parallel_a");
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn supervisor_cancellation_terminates_native_executor_and_codex_descendants() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    directory.install_codex_stub();
    directory.configure_native(fake.url().as_str(), 1);
    let child_pid_file = directory.path.join("native-codex-child.pid");
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_native_cancel", 5, vec![json!({
            "name":"FAKE_CODEX_CHILD_PID_FILE",
            "value":child_pid_file,
            "secret":false
        })])],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(15));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read native Codex descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse native Codex descendant PID");
    fake.enqueue_poll(json!({
        "assignments": [],
        "cancel_requests": [{"run_id":"arun_native_cancel","token":"native-cancel-token"}],
        "cancels": []
    }));

    fake.wait_for(Duration::from_secs(20), |requests| {
        requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["cancellation_acks"]
                    .as_array()
                    .is_some_and(|acks| {
                        acks.iter().any(|ack| {
                            ack["run_id"] == "arun_native_cancel"
                                && ack["token"] == "native-cancel-token"
                        })
                    })
        })
    });
    assert_process_stopped(child_pid);
    assert!(fake.accepted_finishes().is_empty());
    fake.wait_for(Duration::from_secs(10), |requests| {
        requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["owned_runs"] == json!([])
                && request.json()["cancellation_acks"].is_null()
        })
    });
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn tines_end_to_end_acceptance_routes_issue_and_completes_the_run() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, true);
    let local_repository = create_local_repository(&directory.path.join("acceptance-repository"));
    directory.write_events(
        "arun_happy",
        [
            json!({"version":1,"type":"log","stream":"stdout","message":"thread-from-stub started"}),
            json!({"version":1,"type":"session","provider":"codex","id":"thread-from-stub"}),
            json!({"version":1,"type":"usage","input_tokens":75,"output_tokens":7,"cache_read_tokens":20,"cache_write_tokens":5}),
            json!({
                "version":1,
                "type":"result",
                "status":"completed",
                "exit_code":0,
                "provider_session_id":"thread-from-stub",
                "usage":{"input_tokens":75,"output_tokens":7,"cache_read_tokens":20,"cache_write_tokens":5},
                "pricing_evidence":{
                    "provider":"codex",
                    "version":1,
                    "payload":{
                        "version":1,
                        "harness":"codex",
                        "model":"gpt-5.1-codex",
                        "identity_source":"launch_argument",
                        "usage_scope":"thread_total",
                        "session_mode":"cold",
                        "normalization":"codex-jsonl-v1",
                        "model_rerouted":false,
                        "measurement_status":"complete",
                        "terminal_snapshots":1
                    }
                },
                "interrupted":false
            }),
        ],
    );
    let mut routed_assignment = assignment(
        "arun_happy",
        5,
        vec![json!({"name":"DEPLOY_TOKEN","value":"integration-secret","secret":true})],
    );
    routed_assignment["run"]["issue_ref"]["number"] = json!(2301);
    routed_assignment["run"]["issue_ref"]["title"] = json!("Acceptance fixture issue");
    routed_assignment["bundle"]["repos"] = json!([{
        "url": local_repository.to_string_lossy(),
        "branch": null,
        "dir": "materialized"
    }]);
    fake.fail_next_polls(1);
    fake.fail_next_logs(1);
    fake.fail_next_finishes(1);
    fake.route_issue("fake-tines-integration", routed_assignment);

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(20));
    let finish = &finishes[0];
    assert_eq!(finish["status"], "completed", "finish state reaches Tines");
    assert_eq!(finish["provider_session_id"], "thread-from-stub");
    assert_eq!(finish["usage"]["input_tokens"], 75);
    assert_eq!(finish["usage"]["cache_read_tokens"], 20);
    assert_eq!(finish["usage"]["cache_write_tokens"], 5);
    assert_eq!(finish["usage"]["output_tokens"], 7);
    let captures = directory.path.join("captures");
    assert_eq!(
        fs::read_to_string(captures.join("arun_happy.executor"))
            .expect("read selected executor marker")
            .trim(),
        "selected",
        "project, workflow, and state selectors choose the executor override"
    );
    let executor_request: Value = serde_json::from_slice(
        &fs::read(captures.join("arun_happy.request.json")).expect("read executor request"),
    )
    .expect("decode executor request");
    assert_eq!(
        executor_request["assignment"]["prompt"],
        "exercise arun_happy"
    );
    assert_eq!(
        executor_request["assignment"]["run_key"],
        "issue-run-key-arun_happy"
    );
    assert_eq!(
        executor_request["assignment"]["env"][0]["value"],
        "integration-secret"
    );
    assert_eq!(
        executor_request["assignment"]["bundle"]["repos"][0]["dir"],
        "materialized"
    );

    wait_for_quiet_poll(&fake, "arun_happy");
    stop_gracefully(&fake, &mut runner);

    let requests = fake.requests();
    let registration = requests
        .iter()
        .find(|request| request.target == "/api/v1/runners/register")
        .expect("runner registration request");
    assert_eq!(
        registration.header("authorization"),
        Some("Bearer fake-bootstrap-key")
    );
    assert_eq!(
        registration.json()["name"],
        "fake-tines-integration",
        "Tines routes the issue using the registered local runner name"
    );
    let poll_attempts = poll_requests(&requests);
    assert!(
        poll_attempts.len() >= 4,
        "poll retry and lifecycle requests were recorded"
    );
    assert_eq!(
        poll_attempts[0].json()["effort_capabilities"]["harness_version"],
        "codex-fake 0.1.0"
    );
    let metadata = requests
        .iter()
        .find(|request| request.target == "/api/v1/issues/iss_arun_happy")
        .expect("issue metadata request");
    assert_eq!(
        metadata.header("authorization"),
        Some("Bearer issue-run-key-arun_happy"),
        "issue detail lookup uses the assignment run key"
    );
    let issue_detail_requests = requests
        .iter()
        .filter(|request| request.target == "/api/v1/issues/iss_arun_happy")
        .collect::<Vec<_>>();
    assert_eq!(
        issue_detail_requests.len(),
        1,
        "the daemon resolves issue metadata before it launches the executor"
    );
    assert!(issue_detail_requests.iter().all(|request| {
        request.header("authorization") == Some("Bearer issue-run-key-arun_happy")
    }));

    let log_attempts = requests
        .iter()
        .filter(|request| request.target == "/api/v1/runs/arun_happy/logs")
        .collect::<Vec<_>>();
    assert!(
        log_attempts.len() >= 3,
        "log failure was retried before finish"
    );
    assert_eq!(log_attempts[0].json()["seq"], log_attempts[1].json()["seq"]);
    assert_eq!(
        log_attempts[0].json()["chunk"],
        log_attempts[1].json()["chunk"]
    );
    let accepted_logs = fake.accepted_logs();
    let accepted_log_output = accepted_logs
        .iter()
        .filter_map(|log| log["chunk"].as_str())
        .collect::<String>();
    assert!(accepted_log_output.contains("thread-from-stub"));
    assert!(!accepted_log_output.contains("integration-secret"));
    assert!(
        requests.iter().any(|request| {
            request.target == "/api/v1/runs/arun_happy/logs"
                && request.header("authorization") == Some("Bearer fake-runner-token")
        }),
        "streamed logs use the registered runner token"
    );

    let finish_attempts = requests
        .iter()
        .filter(|request| request.target == "/api/v1/runs/arun_happy/finish")
        .collect::<Vec<_>>();
    assert_eq!(
        finish_attempts.len(),
        2,
        "transient finish failure was retried"
    );
    assert_eq!(finish_attempts[0].json(), finish_attempts[1].json());
    assert!(
        !directory.workspace_parent().exists(),
        "generic executor runs do not create a daemon workspace"
    );
    assert!(
        fake.unexpected_requests().is_empty(),
        "unexpected protocol requests: {:?}",
        fake.unexpected_requests()
    );
}

#[test]
fn stub_rate_limit_jsonl_is_reported_as_a_failed_rate_limited_run() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    directory.write_events(
        "arun_rate_limit",
        [
            json!({"version":1,"type":"session","provider":"codex","id":"thread-limited"}),
            json!({
                "version":1,
                "type":"result",
                "status":"rate_limited",
                "exit_code":17,
                "error":"Rate limit exceeded.",
                "provider_session_id":"thread-limited",
                "rate_limit":{"resume_at":2_000_000_000_000_u64,"message":"Rate limit exceeded."},
                "interrupted":false
            }),
        ],
    );
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_rate_limit", 5, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(20));
    assert_eq!(finishes[0]["status"], "failed");
    assert_eq!(finishes[0]["judgment"], "rate_limited");
    assert_eq!(finishes[0]["resume_at"], 2_000_000_000_000_u64);
    assert_eq!(finishes[0]["provider_session_id"], "thread-limited");
    wait_for_quiet_poll(&fake, "arun_rate_limit");
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn malformed_executor_output_fails_without_leaking_assignment_secrets() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    let events = directory.path.join("events");
    fs::write(
        events.join("arun_broken.jsonl"),
        "not-json issue-run-key-arun_broken\n",
    )
    .expect("write malformed executor output");
    fs::write(
        directory.path.join("control/arun_broken.stderr"),
        "executor diagnostic issue-run-key-arun_broken\n",
    )
    .expect("write executor stderr diagnostic");
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_broken", 5, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(20));
    assert_eq!(finishes[0]["status"], "failed");
    let error = finishes[0]["error"].as_str().expect("finish error");
    assert!(error.contains("malformed executor JSONL at line 1"));
    assert!(error.contains("Executor stderr:"));
    assert!(error.contains("[REDACTED]"));
    assert!(!error.contains("issue-run-key-arun_broken"));
    wait_for_quiet_poll(&fake, "arun_broken");
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn concurrent_assignments_run_in_parallel_and_finish_independently() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 2, false);
    let control = directory.path.join("control");
    let barrier = control.join("barrier");
    let assignments = ["arun_parallel_a", "arun_parallel_b"]
        .into_iter()
        .map(|run_id| {
            fs::write(control.join(format!("{run_id}.barrier")), "2")
                .expect("configure executor concurrency barrier");
            assignment(run_id, 5, Vec::new())
        })
        .collect::<Vec<_>>();
    fake.enqueue_poll(json!({"assignments": assignments, "cancels": []}));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(2, Duration::from_secs(20));
    assert_eq!(finishes.len(), 2);
    assert!(
        finishes
            .iter()
            .all(|finish| finish["status"] == "completed")
    );
    assert!(barrier.join("arun_parallel_a").exists());
    assert!(barrier.join("arun_parallel_b").exists());
    let first_poll = poll_requests(&fake.requests())[0].json();
    assert_eq!(first_poll["max_concurrent"], 2);
    wait_for_quiet_poll(&fake, "arun_parallel_a");
    stop_gracefully(&fake, &mut runner);
    let finishes = fake
        .requests()
        .into_iter()
        .filter(|request| request.target.ends_with("/finish"))
        .map(|request| request.target)
        .collect::<Vec<_>>();
    assert!(finishes.contains(&"/api/v1/runs/arun_parallel_a/finish".to_owned()));
    assert!(finishes.contains(&"/api/v1/runs/arun_parallel_b/finish".to_owned()));
}

#[test]
fn supervisor_cancellation_kills_stub_descendants_and_is_acknowledged() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    let control = directory.path.join("control");
    fs::write(control.join("arun_cancel.child"), "spawn child")
        .expect("configure executor child process");
    let child_pid_file = control.join("arun_cancel.pid");
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_cancel", 5, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read stub descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse stub descendant PID");
    fake.enqueue_poll(json!({
        "assignments": [],
        "cancel_requests": [{"run_id": "arun_cancel", "token": "cancel-token"}],
        "cancels": []
    }));

    fake.wait_for(Duration::from_secs(15), |requests| {
        requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["cancellation_acks"]
                    .as_array()
                    .is_some_and(|acks| {
                        acks.iter().any(|ack| {
                            ack["run_id"] == "arun_cancel" && ack["token"] == "cancel-token"
                        })
                    })
        })
    });
    assert_process_stopped(child_pid);
    assert!(
        fake.accepted_finishes().is_empty(),
        "supervisor cancellation omits finish"
    );
    fake.wait_for(Duration::from_secs(10), |requests| {
        requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["owned_runs"] == json!([])
                && request.json()["cancellation_acks"].is_null()
        })
    });
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn cancellation_after_executor_result_does_not_report_a_second_finish() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    let run_id = "arun_cancel_after_result";
    let control = directory.path.join("control");
    fs::write(
        control.join(format!("{run_id}.child")),
        "keep transport open",
    )
    .expect("configure executor process-group child");
    directory.write_events(
        run_id,
        [
            json!({"version":1,"type":"log","stream":"system","message":"executor emitted terminal result and is still waiting while the daemon keeps transport state durable and the child remains alive"}),
            json!({"version":1,"type":"result","status":"completed","exit_code":0,"interrupted":false}),
        ],
    );
    let child_pid_file = control.join(format!("{run_id}.pid"));
    fake.enqueue_poll(json!({
        "assignments": [assignment(run_id, 5, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read executor descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse descendant PID");
    wait_for_file(
        &control.join(format!("{run_id}.events-sent")),
        Duration::from_secs(10),
    );
    wait_for_run_log(&fake, run_id, "executor emitted terminal result");
    fake.enqueue_poll(json!({
        "assignments": [],
        "cancel_requests": [{"run_id": run_id, "token": "cancel-after-result"}],
        "cancels": []
    }));
    fake.wait_for(Duration::from_secs(15), |requests| {
        requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["cancellation_acks"]
                    .as_array()
                    .is_some_and(|acks| {
                        acks.iter().any(|ack| {
                            ack["run_id"] == run_id && ack["token"] == "cancel-after-result"
                        })
                    })
        })
    });

    assert_process_stopped(child_pid);
    assert!(
        fake.accepted_finishes().is_empty(),
        "Tines cancellation after an executor result must suppress finish reporting"
    );
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn sigterm_stops_the_executor_transport_and_reports_one_interrupted_finish() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    let run_id = "arun_sigterm_active";
    let control = directory.path.join("control");
    fs::write(
        control.join(format!("{run_id}.child")),
        "keep transport open",
    )
    .expect("configure executor process-group child");
    directory.write_events(
        run_id,
        [json!({"version":1,"type":"log","stream":"system","message":"executor is active and waiting for a supervisor signal while its transport and descendant remain alive"})],
    );
    let child_pid_file = control.join(format!("{run_id}.pid"));
    fake.enqueue_poll(json!({
        "assignments": [assignment(run_id, 5, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read executor descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse descendant PID");
    let active_runs_path = directory.active_runs_path();
    let transport_identity =
        wait_for_process_identity(&active_runs_path, run_id, Duration::from_secs(10));
    assert!(
        !directory
            .credentials_path()
            .with_file_name("active-runs.json")
            .exists(),
        "active-run records stay under state_dir"
    );
    let transport_pid = transport_identity["process_id"]
        .as_u64()
        .expect("persisted transport process ID") as u32;
    wait_for_run_log(&fake, run_id, "executor is active and waiting");

    runner.signal("TERM");
    let status = runner.wait(Duration::from_secs(10));
    assert!(status.success(), "graceful shutdown failed: {status}");
    assert_process_stopped(transport_pid);
    assert_process_stopped(child_pid);
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(10));
    assert_eq!(finishes.len(), 1, "shutdown reports the run once");
    assert_eq!(finishes[0]["judgment"], "interrupted");
}

#[test]
fn zero_minute_assignment_timeout_uses_the_protocol_finish_path() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    fs::write(directory.path.join("control/arun_timeout.sleep"), "30")
        .expect("configure executor timeout");
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_timeout", 0, Vec::new())],
        "cancels": []
    }));

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let finishes = fake.wait_for_finishes(1, Duration::from_secs(20));
    assert_eq!(finishes[0]["status"], "failed");
    assert!(
        finishes[0]["error"]
            .as_str()
            .unwrap()
            .contains("0-minute run timeout")
    );
    wait_for_quiet_poll(&fake, "arun_timeout");
    stop_gracefully(&fake, &mut runner);
}

#[test]
fn restart_recovers_a_crashed_run_before_polling_with_empty_ownership() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    let control = directory.path.join("control");
    fs::write(control.join("arun_crash.child"), "spawn child")
        .expect("configure executor child process");
    let child_pid_file = control.join("arun_crash.pid");
    fake.enqueue_poll(json!({
        "assignments": [assignment("arun_crash", 5, Vec::new())],
        "cancels": []
    }));

    let mut first = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read crashed stub descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse crashed stub descendant PID");
    let active_runs_path = directory.active_runs_path();
    let transport_identity =
        wait_for_process_identity(&active_runs_path, "arun_crash", Duration::from_secs(10));
    let process_group_id = transport_identity["process_group_id"]
        .as_u64()
        .expect("persisted executor transport process group ID") as u32;
    #[cfg(target_os = "linux")]
    assert_eq!(
        linux_process_group_id(child_pid),
        Some(process_group_id),
        "crash fixture child belongs to the persisted executor transport group"
    );
    let first_requests = fake.requests();
    let first_polls = poll_requests(&first_requests);
    let first_instance = first_polls
        .iter()
        .find_map(|request| request.json()["instance_id"].as_str().map(str::to_owned))
        .expect("first daemon instance id");
    assert!(
        is_process_running(child_pid),
        "crash fixture child is live before restart"
    );
    let crashed_status = first.kill_now();
    assert!(
        !crashed_status.success(),
        "first daemon was killed to simulate a crash"
    );
    assert!(directory.active_runs_path().exists());

    let mut restarted = directory.runner(None);
    fake.wait_for(Duration::from_secs(10), |requests| {
        poll_requests(requests).iter().any(|request| {
            let body = request.json();
            body["instance_id"]
                .as_str()
                .is_some_and(|value| value != first_instance)
                && body["owned_runs"] == json!([])
        })
    });
    assert_process_stopped(child_pid);
    assert!(
        fake.accepted_finishes().is_empty(),
        "recovery leaves reconciliation to Tines"
    );
    stop_gracefully(&fake, &mut restarted);
}

#[test]
fn duplicate_daemon_cannot_recover_a_live_registration_or_start_from_credential_alias() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    let server_url = format!("{}/tines", fake.url());
    directory.configure(&server_url, &stub, 1, false);
    let run_id = "arun_duplicate_daemon";
    let control = directory.path.join("control");
    fs::write(
        control.join(format!("{run_id}.child")),
        "keep transport open",
    )
    .expect("configure executor descendant");
    directory.write_events(
        run_id,
        [json!({"version":1,"type":"log","stream":"system","message":"active executor must remain untouched while duplicate startup is rejected"})],
    );
    fake.route_issue("fake-tines-integration", assignment(run_id, 5, Vec::new()));

    let mut first = directory.runner(Some("fake-bootstrap-key"));
    let child_pid_file = control.join(format!("{run_id}.pid"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read executor descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse executor descendant PID");
    let active_runs_path = directory.active_runs_path();
    let transport_identity =
        wait_for_process_identity(&active_runs_path, run_id, Duration::from_secs(10));
    let transport_pid = transport_identity["process_id"]
        .as_u64()
        .expect("persisted executor transport process ID") as u32;
    wait_for_run_log(&fake, run_id, "active executor must remain untouched");
    let original_state = fs::read(&active_runs_path).expect("read live active-run state");
    let first_instance = poll_requests(&fake.requests())
        .iter()
        .find_map(|request| request.json()["instance_id"].as_str().map(str::to_owned))
        .expect("first daemon poll instance ID");

    let config_path = directory.path.join("config/tines-runner-rs/config.toml");
    let duplicate_config_path = directory.path.join("duplicate-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("read original runner config");
    let trailing_slash_url = format!("{server_url}/");
    let alternate_config = original_config.replace(
        &format!("url = {server_url:?}"),
        &format!("url = {trailing_slash_url:?}"),
    );
    assert_ne!(
        alternate_config, original_config,
        "replace the configured API URL"
    );
    fs::write(&duplicate_config_path, alternate_config)
        .expect("write equivalent trailing-slash runner config");
    let mut duplicate_command = directory.runner_command(&duplicate_config_path, None, None);
    duplicate_command.stdout(Stdio::piped());
    duplicate_command.stderr(Stdio::piped());
    let duplicate = RunnerProcess {
        child: duplicate_command.spawn().expect("start duplicate daemon"),
    };
    let output = duplicate.wait_with_output(Duration::from_secs(10));
    assert!(
        !output.status.success(),
        "duplicate daemon must fail to start"
    );
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("another local daemon already owns this runner registration"),
        "duplicate startup should explain the ownership conflict: {diagnostic}"
    );
    assert_eq!(
        fs::read(&active_runs_path).expect("read active-run state after duplicate startup"),
        original_state,
        "duplicate startup must not change the active-run state"
    );
    assert!(
        is_process_running(transport_pid),
        "executor transport remains live"
    );
    assert!(
        is_process_running(child_pid),
        "executor descendant remains live"
    );
    assert!(
        poll_requests(&fake.requests())
            .iter()
            .all(|request| { request.json()["instance_id"] == first_instance }),
        "a rejected daemon must not start polling"
    );

    let alias_credentials = directory.path.join("credential-alias/credentials.toml");
    fs::create_dir_all(
        alias_credentials
            .parent()
            .expect("alias credentials have a parent"),
    )
    .expect("create credential alias directory");
    fs::copy(directory.credentials_path(), &alias_credentials)
        .expect("copy runner credentials to an alias path");
    let alias_config_path = directory.path.join("credential-alias/config.toml");
    let alias_config = format!(
        "[server]\nurl = {:?}\n[runner]\nname = \"fake-tines-integration\"\nexecutor = {}\nexecutor_cwd = \"~\"\nworkspace_parent = {:?}\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {:?}\nstate_dir = {:?}\n",
        trailing_slash_url,
        executor(
            &stub,
            "alias",
            &directory.path.join("events"),
            &directory.path.join("captures"),
            &directory.path.join("control")
        ),
        directory.workspace_parent(),
        alias_credentials,
        directory.path.join("state")
    );
    fs::write(&alias_config_path, alias_config).expect("write alias config");
    let mut alias_command = directory.runner_command(&alias_config_path, None, None);
    alias_command.stdout(Stdio::piped());
    alias_command.stderr(Stdio::piped());
    let alias = RunnerProcess {
        child: alias_command
            .spawn()
            .expect("start daemon with credential alias"),
    };
    let output = alias.wait_with_output(Duration::from_secs(10));
    assert!(
        !output.status.success(),
        "credential alias must share ownership"
    );
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("another local daemon already owns this runner registration"),
        "credential alias startup should explain the ownership conflict: {diagnostic}"
    );
    assert_eq!(
        fs::read(&active_runs_path).expect("read active-run state after alias startup"),
        original_state,
        "credential alias startup must not change the active-run state"
    );
    assert!(
        is_process_running(transport_pid),
        "executor transport remains live"
    );
    assert!(
        is_process_running(child_pid),
        "executor descendant remains live"
    );
    assert!(
        poll_requests(&fake.requests())
            .iter()
            .all(|request| { request.json()["instance_id"] == first_instance }),
        "a credential alias must not start polling"
    );

    stop_gracefully(&fake, &mut first);
}

#[test]
fn distinct_named_runner_registrations_can_poll_concurrently() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    let config_path = directory.path.join("shared-runners.toml");
    let credentials_codex = directory.path.join("codex/credentials.toml");
    let credentials_antigravity = directory.path.join("antigravity/credentials.toml");
    let events = directory.path.join("events");
    let captures = directory.path.join("captures");
    let control = directory.path.join("control");
    for path in [&events, &captures, &control] {
        fs::create_dir_all(path).expect("create named-runner fixture directory");
    }
    let codex_executor = executor(&stub, "codex", &events, &captures, &control);
    let antigravity_executor = executor(&stub, "antigravity", &events, &captures, &control);
    let config = format!(
        "[server]\nurl = {:?}\n\n[runners.codex]\nname = \"shared-codex\"\ncredentials_file = {:?}\nexecutor = {}\nexecutor_cwd = \"~\"\nworkspace_parent = {:?}\nmax_concurrent = 2\npoll_interval_seconds = 1\n\n[runners.antigravity]\nname = \"shared-antigravity\"\ncredentials_file = {:?}\nrunner_type = \"custom\"\ncustom_command = [\"antigravity\"]\nexecutor = {}\nexecutor_cwd = \"~\"\nworkspace_parent = {:?}\nmax_concurrent = 4\npoll_interval_seconds = 1\n\n[storage]\nstate_dir = {:?}\n",
        fake.url(),
        credentials_codex,
        codex_executor,
        directory.workspace_parent(),
        credentials_antigravity,
        antigravity_executor,
        directory.workspace_parent(),
        directory.path.join("state")
    );
    fs::write(&config_path, config).expect("write shared named-runner config");

    let mut codex = directory.runner_for(&config_path, Some("codex"), Some("fake-bootstrap-key"));
    fake.wait_for(Duration::from_secs(10), |requests| {
        poll_requests(requests)
            .iter()
            .any(|request| request.target.contains("rnr_fake_tines_1"))
    });
    let mut antigravity = directory.runner_for(
        &config_path,
        Some("antigravity"),
        Some("fake-bootstrap-key"),
    );
    fake.wait_for(Duration::from_secs(10), |requests| {
        let registrations = requests
            .iter()
            .filter(|request| request.target == "/api/v1/runners/register")
            .collect::<Vec<_>>();
        let poll_targets = poll_requests(requests)
            .iter()
            .map(|request| request.target.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        registrations.len() >= 2 && poll_targets.len() >= 2
    });
    assert!(directory.path.join("state/runner-codex").is_dir());
    assert!(directory.path.join("state/runner-antigravity").is_dir());
    codex.assert_running();
    antigravity.assert_running();

    let registrations = fake
        .requests()
        .into_iter()
        .filter(|request| request.target == "/api/v1/runners/register")
        .map(|request| request.json())
        .collect::<Vec<_>>();
    assert!(
        registrations
            .iter()
            .any(|body| { body["name"] == "shared-codex" && body["max_concurrent"] == 2 })
    );
    assert!(
        registrations
            .iter()
            .any(|body| { body["name"] == "shared-antigravity" && body["max_concurrent"] == 4 })
    );
    let requests = fake.requests();
    let poll_targets = poll_requests(&requests)
        .iter()
        .map(|request| request.target.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        poll_targets.len(),
        2,
        "each registration polls independently"
    );

    stop_gracefully(&fake, &mut codex);
    stop_gracefully(&fake, &mut antigravity);
}

#[test]
fn daemon_fencing_response_is_terminal() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    fake.enqueue_poll_response(
        409,
        json!({"error": {"code": "runner_conflict", "message": "another daemon instance is serving this runner; this one has been superseded"}}),
    );

    let mut runner = directory.runner(Some("fake-bootstrap-key"));
    let status = runner.wait(Duration::from_secs(10));
    assert!(!status.success(), "fenced runner exits with failure");
    let requests = fake.requests();
    assert_eq!(poll_requests(&requests).len(), 1);
    assert!(
        !requests
            .iter()
            .any(|request| request.target.ends_with("/finish"))
    );
}

fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn wait_for_process_identity(path: &std::path::Path, run_id: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let identity = fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
            .and_then(|state| state["runs"][run_id]["transport"].as_object().cloned());
        if let Some(identity) = identity {
            assert!(identity["process_id"].as_u64().is_some());
            assert!(identity["process_group_id"].as_u64().is_some());
            return Value::Object(identity);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the executor transport identity in {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
fn assert_process_stopped(process_id: u32) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match fs::read_to_string(format!("/proc/{process_id}/stat")) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(") ")
                    .expect("valid proc stat record")
                    .1
                    .chars()
                    .next()
                    .expect("process state");
                if matches!(state, 'Z' | 'X') {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("inspect process {process_id}: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "process {process_id} remained live: {}",
            fs::read_to_string(format!("/proc/{process_id}/stat")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn assert_process_stopped(process_id: u32) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if !Command::new("kill")
            .args(["-0", &process_id.to_string()])
            .status()
            .expect("check child process")
            .success()
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("process {process_id} remained live");
}

#[cfg(target_os = "linux")]
fn is_process_running(process_id: u32) -> bool {
    fs::read_to_string(format!("/proc/{process_id}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !matches!(rest.chars().next(), Some('Z' | 'X')))
    })
}

#[cfg(target_os = "linux")]
fn linux_process_group_id(process_id: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{process_id}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_process_running(process_id: u32) -> bool {
    Command::new("kill")
        .args(["-0", &process_id.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}
