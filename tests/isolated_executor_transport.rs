#![cfg(unix)]

mod support {
    #[allow(dead_code)]
    pub mod fake_tines;
}

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_tines::{FakeTines, RecordedRequest};
use tines_runner_rs::protocol::RunnerAssignment;
use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tines-runner-isolated-transport-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("create isolated transport test directory");
        Self(path)
    }

    fn prepare(&self, server_url: &str) -> FixturePaths {
        let captures = self.0.join("transport-captures");
        let executor_bin = self.0.join("executor-environment/bin");
        let executor_home = self.0.join("executor-environment/home");
        let daemon_bin = self.0.join("daemon-environment/bin");
        let capability_cwd = self.0.join("transport-cwds/default");
        let first_cwd = self.0.join("transport-cwds/first");
        let second_cwd = self.0.join("transport-cwds/second");
        let daemon_workspaces = self.0.join("daemon-workspaces");
        let first_workspaces = self.0.join("executor-environment/workspaces/first");
        let second_workspaces = self.0.join("executor-environment/workspaces/second");
        for path in [
            &captures,
            &executor_bin,
            &executor_home,
            &daemon_bin,
            &capability_cwd,
            &first_cwd,
            &second_cwd,
            &daemon_workspaces,
            &first_workspaces,
            &second_workspaces,
        ] {
            fs::create_dir_all(path).expect("create fixture directory");
        }

        let python = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .expect("find Python 3 for the transport shim");
        assert!(
            python.status.success(),
            "Python 3 is required by this acceptance test"
        );
        let python = String::from_utf8(python.stdout)
            .expect("Python executable path is UTF-8")
            .trim()
            .to_owned();

        let shim = self.0.join("isolated_executor_transport.py");
        fs::write(
            &shim,
            include_str!("support/isolated_executor_transport.py"),
        )
        .expect("write the transport shim");
        let harness = executor_bin.join("codex");
        fs::write(&harness, include_str!("support/isolated_codex.sh"))
            .expect("write the isolated Codex stub");
        fs::set_permissions(&harness, fs::Permissions::from_mode(0o755))
            .expect("make the Codex stub executable");

        let runner_binary = Path::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
        let executor_argv = serde_json::to_string(&[
            python,
            shim.to_string_lossy().into_owned(),
            captures.to_string_lossy().into_owned(),
            runner_binary.to_string_lossy().into_owned(),
            executor_bin.to_string_lossy().into_owned(),
            executor_home.to_string_lossy().into_owned(),
        ])
        .expect("encode shim argv as TOML-compatible JSON");
        let path_value = |path: &Path| {
            serde_json::to_string(path.to_string_lossy().as_ref())
                .expect("encode fixture path as a TOML string")
        };
        let config_dir = self.0.join("config/tines-runner-rs");
        fs::create_dir_all(&config_dir).expect("create isolated runner config directory");
        let credentials = self.0.join("daemon-state/credentials.toml");
        fs::create_dir_all(credentials.parent().unwrap()).expect("create daemon state directory");
        let config = format!(
            "[server]\nurl = {}\n[runner]\nname = \"isolated-transport-acceptance\"\nexecutor = {}\nexecutor_cwd = {}\nworkspace_parent = {}\nmax_concurrent = 1\npoll_interval_seconds = 1\n[storage]\ncredentials_file = {}\nkeep_workspaces = \"never\"\n\n[[override]]\nproject = \"Tines\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nexecutor_cwd = {}\nworkspace_parent = {}\n\n[[override]]\nproject = \"Alternate\"\nworkflow = \"Implementation\"\nstate = \"Implement\"\nexecutor_cwd = {}\nworkspace_parent = {}\nrun_key_delivery = \"environment\"\n",
            path_value(Path::new(server_url)),
            executor_argv,
            path_value(&capability_cwd),
            path_value(&daemon_workspaces),
            path_value(&credentials),
            path_value(&first_cwd),
            path_value(&first_workspaces),
            path_value(&second_cwd),
            path_value(&second_workspaces),
        );
        fs::write(config_dir.join("config.toml"), config).expect("write runner config");

        let daemon_path = std::env::join_paths([
            daemon_bin.as_os_str(),
            std::ffi::OsStr::new("/usr/bin"),
            std::ffi::OsStr::new("/bin"),
        ])
        .expect("compose daemon PATH without the executor's private bin directory")
        .to_string_lossy()
        .into_owned();

        FixturePaths {
            captures,
            daemon_path,
            daemon_workspaces,
            capability_cwd,
            first_cwd,
            second_cwd,
            first_workspaces,
            second_workspaces,
            credentials,
            repository: create_local_repository(&self.0.join("source-repository")),
            log: self.0.join("daemon-stderr.log"),
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FixturePaths {
    captures: PathBuf,
    daemon_path: String,
    daemon_workspaces: PathBuf,
    capability_cwd: PathBuf,
    first_cwd: PathBuf,
    second_cwd: PathBuf,
    first_workspaces: PathBuf,
    second_workspaces: PathBuf,
    credentials: PathBuf,
    repository: PathBuf,
    log: PathBuf,
}

struct RunnerProcess {
    child: Child,
}

impl RunnerProcess {
    fn start(directory: &TestDirectory, paths: &FixturePaths) -> Self {
        let runner_binary = Path::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
        let log = File::create(&paths.log).expect("create daemon stderr log");
        let child = Command::new(runner_binary)
            .env("HOME", &directory.0)
            .env("USERPROFILE", &directory.0)
            .env("XDG_CONFIG_HOME", directory.0.join("config"))
            .env("XDG_DATA_HOME", directory.0.join("data"))
            .env("APPDATA", &directory.0)
            .env("LOCALAPPDATA", &directory.0)
            .env("PATH", &paths.daemon_path)
            .env("RUST_LOG", "warn")
            .env("TRANSPORT_DAEMON_MARKER", "host-only-marker")
            .env("TINES_API_KEY", "fake-bootstrap-key")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start daemon with no Codex on PATH");
        Self { child }
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .args([format!("-{signal}"), self.child.id().to_string()])
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
}

impl Drop for RunnerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(8);
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

fn assignment(
    paths: &FixturePaths,
    run_id: &str,
    project: &str,
    timeout_minutes: u64,
    mode: &str,
) -> Value {
    let harness_capture = paths.captures.join(format!("{run_id}.workspace.txt"));
    let harness_pid = paths.captures.join(format!("{run_id}.harness.pid"));
    let descendant_pid = paths.captures.join(format!("{run_id}.descendant.pid"));
    json!({
        "run": {
            "id": run_id,
            "issue_id": format!("iss_{run_id}"),
            "issue_ref": {
                "project_name": project,
                "number": 40,
                "title": "Isolated executor transport acceptance"
            },
            "state_at_start_name": "Implement",
            "model": "gpt-5.1-codex"
        },
        "prompt": "Keep this quoted: \"isolated\"; Unicode stays intact: café 🧪.",
        "bundle": {
            "skills": [{
                "name": "transport-skill",
                "files": [{
                    "path": "SKILL.md",
                    "content": "executor-only skill content\n"
                }]
            }],
            "repos": [{
                "url": paths.repository.to_string_lossy(),
                "branch": null,
                "dir": "materialized"
            }]
        },
        "run_key": format!("issue-run-key-{run_id}"),
        "timeout_minutes": timeout_minutes,
        "env": [
            {"name":"TRANSPORT_HARNESS_CAPTURE","value":harness_capture,"secret":false},
            {"name":"TRANSPORT_HARNESS_MODE","value":mode,"secret":false},
            {"name":"TRANSPORT_HARNESS_PID_FILE","value":harness_pid,"secret":false},
            {"name":"TRANSPORT_HARNESS_CHILD_PID_FILE","value":descendant_pid,"secret":false},
            {"name":"DEPLOY_TOKEN","value":"assignment-secret-for-transport","secret":true}
        ]
    })
}

fn create_local_repository(directory: &Path) -> PathBuf {
    fs::create_dir_all(directory).expect("create local acceptance repository");
    fs::write(
        directory.join("acceptance.txt"),
        "repository materialized inside the executor workspace\n",
    )
    .expect("write local repository fixture");
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.name", "Transport Acceptance"][..],
        &["config", "user.email", "transport@example.invalid"][..],
        &["add", "acceptance.txt"][..],
        &["commit", "--quiet", "-m", "executor repository fixture"][..],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .output()
            .expect("run git to prepare the local acceptance repository");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    directory.to_owned()
}

fn route_assignment(fake: &FakeTines, assignment: Value) {
    fake.route_issue("isolated-transport-acceptance", assignment);
}

fn wait_for_finished_run(fake: &FakeTines, run_id: &str, timeout: Duration) -> Value {
    fake.wait_for(timeout, |requests| {
        requests
            .iter()
            .any(|request| request.target == format!("/api/v1/runs/{run_id}/finish"))
    });
    fake.accepted_finishes()
        .into_iter()
        .find(|finish| finish["run_id"] == run_id || finish["id"] == run_id)
        .or_else(|| {
            fake.requests()
                .into_iter()
                .find(|request| request.target == format!("/api/v1/runs/{run_id}/finish"))
                .map(|request| request.json())
        })
        .expect("accepted finish request")
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn wait_for_json(path: &Path, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read(path)
            && let Ok(value) = serde_json::from_slice(&contents)
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_capability_reports(captures: &Path, timeout: Duration) -> Vec<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let reports = fs::read_dir(captures)
            .expect("read transport captures")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("capabilities-")
            })
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|contents| serde_json::from_slice(&contents).ok())
            .collect::<Vec<_>>();
        if !reports.is_empty() {
            return reports;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for capability probes"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_process_identity(path: &Path, run_id: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let identity = fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
            .and_then(|state| state["runs"][run_id]["transport"].as_object().cloned());
        if let Some(identity) = identity {
            return Value::Object(identity);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for active run {run_id}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn process_is_stopped(process_id: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &process_id.to_string()])
        .output()
        .expect("inspect supervised process");
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    state.is_empty() || state.starts_with('Z') || state.starts_with('X')
}

fn assert_process_stopped(process_id: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_is_stopped(process_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        process_is_stopped(process_id),
        "process {process_id} remains alive"
    );
}

fn assert_process_running(process_id: u32) {
    assert!(
        !process_is_stopped(process_id),
        "process {process_id} stopped before timeout settlement"
    );
}

fn wait_for_run_log(fake: &FakeTines, run_id: &str, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let requests = fake.requests();
        if requests.iter().any(|request| {
            request.target.ends_with(&format!("/runs/{run_id}/logs"))
                && request.json()["chunk"]
                    .as_str()
                    .is_some_and(|chunk| chunk.contains(message))
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for log {message:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn logs_for(fake: &FakeTines, run_id: &str) -> String {
    fake.requests()
        .into_iter()
        .filter(|request| request.target.ends_with(&format!("/runs/{run_id}/logs")))
        .filter_map(|request| request.json()["chunk"].as_str().map(str::to_owned))
        .collect()
}

fn request_by_target<'a>(
    requests: &'a [RecordedRequest],
    target: &str,
) -> Vec<&'a RecordedRequest> {
    requests
        .iter()
        .filter(|request| request.target == target)
        .collect()
}

#[test]
fn isolated_transport_covers_capabilities_overrides_protocol_cancellation_and_timeout() {
    let directory = TestDirectory::new();
    let fake = FakeTines::start();
    let paths = directory.prepare(fake.url().as_str());
    let daemon_codex = Command::new("/bin/sh")
        .args(["-c", "command -v codex"])
        .env("PATH", &paths.daemon_path)
        .output()
        .expect("check daemon PATH");
    assert!(!daemon_codex.status.success(), "daemon PATH exposes Codex");
    let mut runner = RunnerProcess::start(&directory, &paths);

    let first = assignment(&paths, "arun_transport_first", "Tines", 5, "success");
    route_assignment(&fake, first.clone());
    let first_finish =
        wait_for_finished_run(&fake, "arun_transport_first", Duration::from_secs(20));
    assert_eq!(
        first_finish["status"],
        "completed",
        "finish: {first_finish:?}; transport: {}; logs: {:?}; daemon: {}",
        fs::read_to_string(paths.captures.join("arun_transport_first.transport.json"))
            .unwrap_or_default(),
        fake.accepted_logs(),
        fs::read_to_string(&paths.log).unwrap_or_default()
    );
    assert_eq!(first_finish["provider_session_id"], "transport-stub-thread");
    assert_eq!(first_finish["usage"]["input_tokens"], 13);
    assert_eq!(first_finish["usage"]["cache_read_tokens"], 5);
    assert_eq!(first_finish["usage"]["cache_write_tokens"], 2);
    assert_eq!(first_finish["usage"]["output_tokens"], 7);

    let first_transport = wait_for_json(
        &paths.captures.join("arun_transport_first.transport.json"),
        Duration::from_secs(5),
    );
    assert_eq!(
        first_transport["cwd"],
        paths.first_cwd.to_string_lossy().as_ref()
    );
    assert_eq!(first_transport["outer_daemon_marker"], "host-only-marker");
    assert_eq!(first_transport["outer_has_tines_api_key"], false);
    assert_eq!(first_transport["outer_has_assignment_secret"], false);
    assert_eq!(first_transport["executor_marker"], "executor-only");
    assert_eq!(
        first_transport["process_group"], first_transport["pid"],
        "the daemon launches the shim in its own process group"
    );
    assert_eq!(
        first_transport["executor_process_group"], first_transport["executor_pid"],
        "the shim launches the executor in a separate process group"
    );
    assert_ne!(first_transport["executor_pid"], first_transport["pid"]);
    assert_eq!(first_transport["executor_exit_status"], 0);

    let first_request: Value = serde_json::from_slice(
        &fs::read(paths.captures.join("arun_transport_first.request.json"))
            .expect("read request forwarded by the shim"),
    )
    .expect("shim captured one JSON request");
    let expected_assignment = serde_json::to_value(
        serde_json::from_value::<RunnerAssignment>(first.clone())
            .expect("valid assigned request fixture"),
    )
    .expect("serialize normalized assignment");
    assert_eq!(first_request["assignment"], expected_assignment);
    assert_eq!(
        first_request["assignment"]["prompt"],
        "Keep this quoted: \"isolated\"; Unicode stays intact: café 🧪."
    );
    assert_eq!(
        first_request["assignment"]["bundle"]["skills"][0]["files"][0]["content"],
        "executor-only skill content\n"
    );

    let first_workspace =
        fs::read_to_string(paths.captures.join("arun_transport_first.workspace.txt"))
            .expect("read harness workspace capture");
    assert!(first_workspace.contains(&format!("workspace={}", paths.first_workspaces.display())));
    assert!(!first_workspace.contains(&format!("workspace={}", paths.first_cwd.display())));
    assert!(first_workspace.contains("executor_marker=executor-only"));
    assert!(first_workspace.contains("daemon_marker="));
    assert!(first_workspace.contains("executor-only skill content"));
    assert!(first_workspace.contains("repository materialized inside the executor workspace"));
    assert!(first_workspace.contains("café 🧪"));

    wait_for_run_log(&fake, "arun_transport_first", "transport-stub-thread");
    let first_logs = logs_for(&fake, "arun_transport_first");
    assert!(first_logs.contains("[session] started (thread transport-stub-thread)"));
    assert!(first_logs.contains("[session] turn completed"));
    assert!(first_logs.contains("stderr-only-transport-harness-diagnostic"));
    assert!(!first_logs.contains("transport-shim-diagnostic-only"));

    let alternate = assignment(
        &paths,
        "arun_transport_alternate",
        "Alternate",
        5,
        "success",
    );
    route_assignment(&fake, alternate);
    assert_eq!(
        wait_for_finished_run(&fake, "arun_transport_alternate", Duration::from_secs(20),)["status"],
        "completed"
    );
    let alternate_transport = wait_for_json(
        &paths
            .captures
            .join("arun_transport_alternate.transport.json"),
        Duration::from_secs(5),
    );
    assert_eq!(
        alternate_transport["cwd"],
        paths.second_cwd.to_string_lossy().as_ref()
    );
    assert_eq!(
        alternate_transport["executor_exit_status"], 0,
        "the shim preserves the nested executor's zero exit status"
    );
    assert!(
        alternate_transport["outer_has_tines_api_key"]
            .as_bool()
            .unwrap()
    );
    assert!(
        alternate_transport["outer_run_key_matches_assignment"]
            .as_bool()
            .unwrap()
    );
    assert!(
        !alternate_transport["request_has_run_key"]
            .as_bool()
            .unwrap()
    );
    assert!(
        !alternate_transport["inner_receives_tines_api_key"]
            .as_bool()
            .unwrap()
    );
    let alternate_request: Value = serde_json::from_slice(
        &fs::read(paths.captures.join("arun_transport_alternate.request.json"))
            .expect("read environment-delivery request"),
    )
    .expect("decode environment-delivery request");
    assert!(alternate_request["assignment"].get("run_key").is_none());
    assert!(
        !alternate_request
            .to_string()
            .contains("issue-run-key-arun_transport_alternate")
    );
    let alternate_workspace = fs::read_to_string(
        paths
            .captures
            .join("arun_transport_alternate.workspace.txt"),
    )
    .expect("read alternate harness workspace capture");
    assert!(
        alternate_workspace.contains(&format!("workspace={}", paths.second_workspaces.display()))
    );
    assert!(alternate_workspace.contains("tines_api_key_present="));
    assert!(!alternate_workspace.contains("tines_api_key_present=x"));
    assert!(alternate_workspace.contains("tines_api_url=http://127.0.0.1:"));
    assert!(!alternate_workspace.contains(&format!("workspace={}", paths.second_cwd.display())));

    let failure = assignment(&paths, "arun_transport_failure", "Tines", 5, "failure");
    route_assignment(&fake, failure);
    let failure_finish =
        wait_for_finished_run(&fake, "arun_transport_failure", Duration::from_secs(20));
    assert_eq!(failure_finish["status"], "failed");
    assert_eq!(
        failure_finish["provider_session_id"],
        "transport-stub-thread"
    );
    let failure_transport = wait_for_json(
        &paths.captures.join("arun_transport_failure.transport.json"),
        Duration::from_secs(5),
    );
    assert_eq!(
        failure_transport["executor_exit_status"], 1,
        "the shim preserves the nested executor's failure status"
    );

    let capability_reports = wait_for_capability_reports(&paths.captures, Duration::from_secs(5));
    let capability_report = capability_reports
        .iter()
        .find(|report| report["cwd"] == paths.capability_cwd.to_string_lossy().as_ref())
        .expect("default executor capability discovery used executor_cwd");
    assert_eq!(
        capability_report["cwd"],
        paths.capability_cwd.to_string_lossy().as_ref()
    );
    assert_eq!(capability_report["outer_path"], paths.daemon_path);
    assert_eq!(capability_report["outer_has_tines_api_key"], false);
    let executor_bin = directory.0.join("executor-environment/bin");
    assert!(
        capability_report["executor_path"]
            .as_str()
            .unwrap()
            .starts_with(&executor_bin.to_string_lossy().to_string())
    );
    let poll = fake
        .requests()
        .into_iter()
        .find(|request| request.target.ends_with("/poll"))
        .expect("runner poll report");
    assert_eq!(
        poll.json()["effort_capabilities"]["harness_version"],
        "transport-codex 0.1.0"
    );

    let cancel = assignment(&paths, "arun_transport_cancel", "Alternate", 5, "cancel");
    route_assignment(&fake, cancel);
    let cancel_harness_pid_file = paths.captures.join("arun_transport_cancel.harness.pid");
    let cancel_descendant_pid_file = paths.captures.join("arun_transport_cancel.descendant.pid");
    wait_for_file(&cancel_harness_pid_file, Duration::from_secs(10));
    wait_for_file(&cancel_descendant_pid_file, Duration::from_secs(10));
    let transport_state = wait_for_process_identity(
        &paths.credentials.with_file_name("active-runs.json"),
        "arun_transport_cancel",
        Duration::from_secs(10),
    );
    let shim_pid = transport_state["process_id"].as_u64().unwrap() as u32;
    let cancel_transport = wait_for_json(
        &paths.captures.join("arun_transport_cancel.transport.json"),
        Duration::from_secs(5),
    );
    let executor_pid = cancel_transport["executor_pid"].as_u64().unwrap() as u32;
    let harness_pid = fs::read_to_string(cancel_harness_pid_file)
        .expect("read Codex PID")
        .trim()
        .parse::<u32>()
        .expect("valid Codex PID");
    let descendant_pid = fs::read_to_string(cancel_descendant_pid_file)
        .expect("read descendant PID")
        .trim()
        .parse::<u32>()
        .expect("valid descendant PID");
    fake.enqueue_poll(json!({
        "assignments": [],
        "cancel_requests": [{"run_id":"arun_transport_cancel","token":"transport-cancel-token"}],
        "cancels": []
    }));
    let cancel_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let requests = fake.requests();
        let acknowledged = requests.iter().any(|request| {
            request.target.ends_with("/poll")
                && request.json()["cancellation_acks"]
                    .as_array()
                    .is_some_and(|acks| {
                        acks.iter().any(|ack| {
                            ack["run_id"] == "arun_transport_cancel"
                                && ack["token"] == "transport-cancel-token"
                        })
                    })
        });
        if acknowledged {
            break;
        }
        assert!(
            Instant::now() < cancel_deadline,
            "cancellation was not acknowledged; requests: {:?}; daemon: {}",
            requests
                .iter()
                .map(|request| &request.target)
                .collect::<Vec<_>>(),
            fs::read_to_string(&paths.log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    for process_id in [shim_pid, executor_pid, harness_pid, descendant_pid] {
        assert_process_stopped(process_id);
    }
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.target != "/api/v1/runs/arun_transport_cancel/finish"),
        "cancellation does not settle the run twice with an ordinary finish"
    );
    let cancellation_ack_count = fake
        .requests()
        .iter()
        .filter(|request| request.target.ends_with("/poll"))
        .map(RecordedRequest::json)
        .flat_map(|request| {
            request["cancellation_acks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|ack| ack["run_id"] == "arun_transport_cancel")
        .count();
    assert_eq!(
        cancellation_ack_count, 1,
        "one cancellation acknowledgement is sent"
    );

    let timeout = assignment(&paths, "arun_transport_timeout", "Tines", 1, "timeout");
    route_assignment(&fake, timeout);
    let timeout_harness_pid_file = paths.captures.join("arun_transport_timeout.harness.pid");
    let timeout_descendant_pid_file = paths.captures.join("arun_transport_timeout.descendant.pid");
    wait_for_file(&timeout_harness_pid_file, Duration::from_secs(10));
    wait_for_file(&timeout_descendant_pid_file, Duration::from_secs(10));
    let timeout_harness_pid = fs::read_to_string(timeout_harness_pid_file)
        .expect("read timeout harness PID")
        .trim()
        .parse::<u32>()
        .expect("valid timeout harness PID");
    let timeout_descendant_pid = fs::read_to_string(timeout_descendant_pid_file)
        .expect("read timeout descendant PID")
        .trim()
        .parse::<u32>()
        .expect("valid timeout descendant PID");
    assert_process_running(timeout_harness_pid);
    assert_process_running(timeout_descendant_pid);

    let timeout_finish =
        wait_for_finished_run(&fake, "arun_transport_timeout", Duration::from_secs(90));
    assert_eq!(timeout_finish["status"], "failed");
    assert!(
        timeout_finish["error"]
            .as_str()
            .unwrap()
            .contains("1-minute run timeout")
    );
    let timeout_transport = wait_for_json(
        &paths.captures.join("arun_transport_timeout.transport.json"),
        Duration::from_secs(5),
    );
    let timeout_shim_pid = timeout_transport["pid"].as_u64().unwrap() as u32;
    let timeout_executor_pid = timeout_transport["executor_pid"].as_u64().unwrap() as u32;
    for process_id in [
        timeout_shim_pid,
        timeout_executor_pid,
        timeout_harness_pid,
        timeout_descendant_pid,
    ] {
        assert_process_stopped(process_id);
    }

    assert_eq!(fs::read_dir(&paths.daemon_workspaces).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&paths.first_workspaces).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&paths.second_workspaces).unwrap().count(), 0);
    runner.signal("TERM");
    assert!(runner.wait(Duration::from_secs(10)).success());

    assert!(!logs_for(&fake, "arun_transport_first").contains("transport-shim-diagnostic-only"));

    let requests = fake.requests();
    for run_id in [
        "arun_transport_first",
        "arun_transport_alternate",
        "arun_transport_failure",
        "arun_transport_timeout",
    ] {
        assert_eq!(
            request_by_target(&requests, &format!("/api/v1/runs/{run_id}/finish")).len(),
            1,
            "one finish request for {run_id}"
        );
    }
}
