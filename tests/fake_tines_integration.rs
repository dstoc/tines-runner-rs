#![cfg(unix)]

mod support {
    pub mod fake_tines;
}

use std::fs;
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
        let path = self.path.join("stub-codex");
        fs::write(&path, include_str!("support/stub_codex.sh")).expect("write Codex stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make Codex stub executable");
        let bin = self.path.join("bin");
        fs::create_dir_all(&bin).expect("create isolated Codex PATH directory");
        let codex = bin.join("codex");
        fs::copy(&path, &codex).expect("install fake Codex CLI");
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
            .expect("make fake Codex CLI executable");
        path
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
        let workspaces = self.path.join("workspaces");
        let default_wrapper = wrapper(stub, "default");
        let executor = serde_json::to_string(&[env!("CARGO_BIN_EXE_tines-runner-rs")])
            .expect("serialize native executor command");
        let override_section = if selected_override {
            format!(
                "\n[[override]]\nproject = \"Tines\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nwrapper = {}\n",
                wrapper(stub, "selected")
            )
        } else {
            String::new()
        };
        let config = format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"fake-tines-integration\"\nexecutor = {executor}\nexecutor_cwd = \"~\"\nwrapper = {default_wrapper}\nworkspace_parent = {:?}\nmax_concurrent = {max_concurrent}\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {:?}\n{override_section}",
            workspaces, credentials
        );
        fs::write(config_dir.join("config.toml"), config).expect("write runner config");
    }

    fn credentials_path(&self) -> std::path::PathBuf {
        self.path.join("credentials.toml")
    }

    fn workspace_parent(&self) -> std::path::PathBuf {
        self.path.join("workspaces")
    }

    fn runner(&self, bootstrap_key: Option<&str>) -> RunnerProcess {
        let config_home = self.path.join("config");
        let data_home = self.path.join("data");
        let mut search_path = vec![self.path.join("bin")];
        search_path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let search_path = std::env::join_paths(search_path).expect("build isolated PATH");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
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
        RunnerProcess {
            child: command.spawn().expect("start runner daemon"),
        }
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

fn wrapper(stub: &std::path::Path, label: &str) -> String {
    serde_json::to_string(&vec![stub.to_string_lossy().into_owned(), label.to_owned()])
        .expect("encode wrapper argv")
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

fn env(name: &str, value: impl Into<String>) -> Value {
    json!({"name": name, "value": value.into(), "secret": false})
}

fn write_jsonl(directory: &TestDirectory, name: &str, contents: &str) -> std::path::PathBuf {
    let path = directory.path.join(name);
    fs::write(&path, contents).expect("write controlled Codex JSONL");
    path
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
fn tines_end_to_end_acceptance_routes_issue_and_completes_the_run() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, true);
    let local_repository = create_local_repository(&directory.path.join("acceptance-repository"));

    let jsonl = write_jsonl(
        &directory,
        "success.jsonl",
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-from-stub\"}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":20,\"cache_write_input_tokens\":5,\"output_tokens\":7}}\n"
        ),
    );
    let wrapper_marker = directory.path.join("wrapper-selected.txt");
    let args_file = directory.path.join("codex-args.txt");
    let workspace_probe = directory.path.join("workspace-probe.txt");
    let run_key_probe = directory.path.join("run-key-api.json");
    let mut routed_assignment = assignment(
        "arun_happy",
        5,
        vec![
            env("FAKE_CODEX_JSONL_FILE", jsonl.to_string_lossy()),
            env("FAKE_CODEX_WRAPPER_FILE", wrapper_marker.to_string_lossy()),
            env("FAKE_CODEX_ARGS_FILE", args_file.to_string_lossy()),
            env(
                "FAKE_CODEX_WORKSPACE_PROBE_FILE",
                workspace_probe.to_string_lossy(),
            ),
            env(
                "FAKE_CODEX_RUN_KEY_PROBE_FILE",
                run_key_probe.to_string_lossy(),
            ),
        ],
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
    assert_eq!(
        fs::read_to_string(&wrapper_marker)
            .expect("read selected wrapper marker")
            .trim(),
        "selected",
        "project, workflow, and state selectors choose the override"
    );
    let workspace_evidence = fs::read_to_string(&workspace_probe)
        .expect("read workspace materialization evidence from the harness");
    assert!(
        workspace_evidence.contains("workspace=")
            && workspace_evidence.contains("prompt=exercise arun_happy")
            && workspace_evidence.contains("repo=materialized from routed test repository"),
        "harness sees the assignment prompt and cloned repository: {workspace_evidence:?}"
    );
    let run_key_response = fs::read_to_string(&run_key_probe)
        .expect("read successful Tines API response fetched by the Codex stub");
    assert!(
        run_key_response.contains("\"id\":\"iss_arun_happy\"")
            && run_key_response.contains("\"name\":\"Implementation\""),
        "Codex stub can use its run key to read issue details: {run_key_response:?}"
    );
    let args = fs::read_to_string(args_file).expect("read captured Codex argv");
    assert!(
        args.starts_with("codex\nexec\n--json\n--skip-git-repo-check\n--model\ngpt-5.1-codex\n")
    );
    assert!(args.ends_with("exercise arun_happy\n"));

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
        2,
        "both the runner and harness use the run-key issue API"
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
    assert!(accepted_logs.iter().any(|log| {
        log["chunk"]
            .as_str()
            .is_some_and(|chunk| chunk.contains("thread-from-stub"))
    }));
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
    assert_eq!(
        fs::read_dir(directory.workspace_parent()).unwrap().count(),
        0
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
    let jsonl = write_jsonl(
        &directory,
        "rate-limit.jsonl",
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-limited\"}\n",
            "{\"type\":\"turn.failed\",\"error\":{\"codex_error_info\":\"rate_limit_exceeded\",\"message\":\"Rate limit exceeded.\",\"retry_at\":2000000000000}}\n"
        ),
    );
    fake.enqueue_poll(json!({
        "assignments": [assignment(
            "arun_rate_limit",
            5,
            vec![
                env("FAKE_CODEX_JSONL_FILE", jsonl.to_string_lossy()),
                env("FAKE_CODEX_WRAPPER_FILE", directory.path.join("wrapper.txt").to_string_lossy()),
                env("FAKE_CODEX_ARGS_FILE", directory.path.join("args.txt").to_string_lossy()),
                env("FAKE_CODEX_EXIT_CODE", "17"),
            ],
        )],
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
fn concurrent_assignments_run_in_parallel_and_finish_independently() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 2, false);
    let barrier = directory.path.join("barrier");
    fs::create_dir_all(&barrier).expect("create Codex concurrency barrier");
    let assignments = ["arun_parallel_a", "arun_parallel_b"]
        .into_iter()
        .map(|run_id| {
            assignment(
                run_id,
                5,
                vec![
                    env("FAKE_CODEX_BARRIER_DIR", barrier.to_string_lossy()),
                    env("FAKE_CODEX_BARRIER_NAME", run_id),
                    env("FAKE_CODEX_BARRIER_COUNT", "2"),
                    env(
                        "FAKE_CODEX_WRAPPER_FILE",
                        directory
                            .path
                            .join(format!("{run_id}.wrapper"))
                            .to_string_lossy(),
                    ),
                    env(
                        "FAKE_CODEX_ARGS_FILE",
                        directory
                            .path
                            .join(format!("{run_id}.args"))
                            .to_string_lossy(),
                    ),
                ],
            )
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
    let child_pid_file = directory.path.join("codex-child.pid");
    fake.enqueue_poll(json!({
        "assignments": [assignment(
            "arun_cancel",
            5,
            vec![
                env("FAKE_CODEX_CHILD_PID_FILE", child_pid_file.to_string_lossy()),
                env("FAKE_CODEX_WRAPPER_FILE", directory.path.join("wrapper.txt").to_string_lossy()),
                env("FAKE_CODEX_ARGS_FILE", directory.path.join("args.txt").to_string_lossy()),
            ],
        )],
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
fn zero_minute_assignment_timeout_uses_the_protocol_finish_path() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let stub = directory.create_stub();
    directory.configure(fake.url().as_str(), &stub, 1, false);
    fake.enqueue_poll(json!({
        "assignments": [assignment(
            "arun_timeout",
            0,
            vec![
                env("FAKE_CODEX_SLEEP_SECONDS", "30"),
                env("FAKE_CODEX_WRAPPER_FILE", directory.path.join("wrapper.txt").to_string_lossy()),
                env("FAKE_CODEX_ARGS_FILE", directory.path.join("args.txt").to_string_lossy()),
            ],
        )],
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
    let child_pid_file = directory.path.join("crash-child.pid");
    fake.enqueue_poll(json!({
        "assignments": [assignment(
            "arun_crash",
            5,
            vec![
                env("FAKE_CODEX_CHILD_PID_FILE", child_pid_file.to_string_lossy()),
                env("FAKE_CODEX_WRAPPER_FILE", directory.path.join("wrapper.txt").to_string_lossy()),
                env("FAKE_CODEX_ARGS_FILE", directory.path.join("args.txt").to_string_lossy()),
            ],
        )],
        "cancels": []
    }));

    let mut first = directory.runner(Some("fake-bootstrap-key"));
    wait_for_file(&child_pid_file, Duration::from_secs(10));
    let child_pid = fs::read_to_string(&child_pid_file)
        .expect("read crashed stub descendant PID")
        .trim()
        .parse::<u32>()
        .expect("parse crashed stub descendant PID");
    let active_runs_path = directory
        .credentials_path()
        .with_file_name("active-runs.json");
    let harness_identity =
        wait_for_process_identity(&active_runs_path, "arun_crash", Duration::from_secs(10));
    let process_group_id = harness_identity["process_group_id"]
        .as_u64()
        .expect("persisted harness process group ID") as u32;
    #[cfg(target_os = "linux")]
    assert_eq!(
        linux_process_group_id(child_pid),
        Some(process_group_id),
        "crash fixture child belongs to the persisted harness group"
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
    assert!(
        directory
            .credentials_path()
            .with_file_name("active-runs.json")
            .exists()
    );

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
            .and_then(|state| state["runs"][run_id]["process"].as_object().cloned());
        if let Some(identity) = identity {
            assert!(identity["process_id"].as_u64().is_some());
            assert!(identity["process_group_id"].as_u64().is_some());
            return Value::Object(identity);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the harness process identity in {}",
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
            "process {process_id} remained live"
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
