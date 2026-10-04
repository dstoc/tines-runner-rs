//! Persistent active-run state and crash recovery.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::WorkspaceRetention;
use crate::process::ProcessIdentity;
use crate::retention;

const STATE_VERSION: u8 = 1;
const ORPHAN_TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// The durable local information needed to recover one active assignment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveRunRecord {
    pub run_id: String,
    #[serde(default)]
    pub process: Option<ProcessIdentity>,
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ActiveRunFile {
    version: u8,
    runs: BTreeMap<String, ActiveRunRecord>,
}

/// An atomically updated store shared by the poll loop and run workers.
#[derive(Clone, Debug)]
pub struct ActiveRunStore {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    path: PathBuf,
    runs: Mutex<BTreeMap<String, ActiveRunRecord>>,
}

impl ActiveRunStore {
    /// Open a state file, or start with an empty state when it does not exist.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let runs = match fs::read(&path) {
            Ok(bytes) => {
                let state: ActiveRunFile = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid active-run state file: {error}"),
                    )
                })?;
                if state.version != STATE_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported active-run state version {}", state.version),
                    ));
                }
                for (run_id, record) in &state.runs {
                    if run_id != &record.run_id
                        || run_id.is_empty()
                        || !record.workspace.is_absolute()
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "active-run state contains an invalid run ID or workspace path",
                        ));
                    }
                }
                state.runs
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };

        Ok(Self {
            inner: Arc::new(StoreInner {
                path,
                runs: Mutex::new(runs),
            }),
        })
    }

    /// Persist a newly launched harness before the caller begins waiting for it.
    pub fn record(
        &self,
        run_id: impl Into<String>,
        process: ProcessIdentity,
        workspace: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.update_record(run_id.into(), Some(process), workspace.as_ref())
    }

    /// Persist the allocated workspace before repository checkout begins.
    pub fn record_workspace(
        &self,
        run_id: impl Into<String>,
        workspace: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.update_record(run_id.into(), None, workspace.as_ref())
    }

    fn update_record(
        &self,
        run_id: String,
        process: Option<ProcessIdentity>,
        workspace: &Path,
    ) -> io::Result<()> {
        let workspace = fs::canonicalize(workspace)?;
        if run_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "active run ID must not be empty",
            ));
        }
        let mut runs = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut updated = runs.clone();
        updated.insert(
            run_id.clone(),
            ActiveRunRecord {
                run_id,
                process,
                workspace,
            },
        );
        write_state(&self.inner.path, &updated)?;
        *runs = updated;
        Ok(())
    }

    /// Remove a locally settled run from persistent state.
    pub fn remove(&self, run_id: &str) -> io::Result<()> {
        let mut runs = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !runs.contains_key(run_id) {
            return Ok(());
        }
        let mut updated = runs.clone();
        updated.remove(run_id);
        write_state(&self.inner.path, &updated)?;
        *runs = updated;
        Ok(())
    }

    /// Return a snapshot of the active records.
    pub fn records(&self) -> Vec<ActiveRunRecord> {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Return the path used for durable state.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
}

/// Stop orphaned harnesses, settle their workspaces, and clear their records.
///
/// The caller must run this before the first poll. The following empty
/// `owned_runs` list lets Tines reconcile any still-active server runs as
/// interrupted.
pub fn recover_active_runs(
    store: &ActiveRunStore,
    retention: &WorkspaceRetention,
    workspace_roots: &[PathBuf],
) -> io::Result<Vec<String>> {
    recover_active_runs_with(store, retention, workspace_roots, |process, grace| {
        process.terminate_if_matches(grace)
    })
}

fn recover_active_runs_with(
    store: &ActiveRunStore,
    retention: &WorkspaceRetention,
    workspace_roots: &[PathBuf],
    mut terminate: impl FnMut(&ProcessIdentity, Duration) -> io::Result<bool>,
) -> io::Result<Vec<String>> {
    let records = store.records();
    let mut recovered = Vec::with_capacity(records.len());
    for record in records {
        validate_workspace_root(&record.workspace, workspace_roots)?;
        let terminated = record
            .process
            .as_ref()
            .map(|process| terminate(process, ORPHAN_TERMINATION_GRACE))
            .transpose()?
            .unwrap_or(false);
        if terminated {
            tracing::warn!(
                run_id = %record.run_id,
                process_id = record.process.as_ref().map(ProcessIdentity::process_id),
                "terminated an orphaned harness during startup recovery"
            );
        } else {
            tracing::info!(
                run_id = %record.run_id,
                process_id = record.process.as_ref().map(ProcessIdentity::process_id),
                "no matching orphan harness remained; leaving any reused PID untouched"
            );
        }

        retention::settle_recovered_workspace(&record.workspace, retention, &record.run_id)
            .map_err(io::Error::other)?;
        if let Some(parent) = record.workspace.parent() {
            retention::prune_retained(parent, retention).map_err(io::Error::other)?;
        }
        store.remove(&record.run_id)?;
        recovered.push(record.run_id);
    }
    Ok(recovered)
}

fn validate_workspace_root(workspace: &Path, roots: &[PathBuf]) -> io::Result<()> {
    let Some(parent) = workspace.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "active-run workspace has no parent directory",
        ));
    };
    for root in roots {
        let root = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if root.is_absolute() {
                    root.clone()
                } else {
                    std::env::current_dir()?.join(root)
                }
            }
            Err(error) => return Err(error),
        };
        if root == parent {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "active-run workspace is outside the configured workspace roots",
    ))
}

fn write_state(path: &Path, runs: &BTreeMap<String, ActiveRunRecord>) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let state = ActiveRunFile {
        version: STATE_VERSION,
        runs: runs.clone(),
    };
    let bytes = serde_json::to_vec(&state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temporary = parent.join(format!(
        ".{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("active-runs"),
        uuid::Uuid::new_v4()
    ));
    let result = write_temporary_state(&temporary, &bytes).and_then(|()| {
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_temporary_state(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ActiveRunStore, recover_active_runs, recover_active_runs_with};
    use crate::config::{RetentionMode, WorkspaceRetention};
    #[cfg(target_os = "macos")]
    use crate::process::ProcessIdentity;
    use crate::process::SupervisedProcess;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("tines-runner-recovery-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn retention(mode: RetentionMode) -> WorkspaceRetention {
        WorkspaceRetention {
            mode,
            max_age: Duration::from_secs(3600),
            max_count: 10,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn recovery_terminates_orphan_and_clears_active_state() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("prompt.md"), "stub").expect("write workspace file");

        let mut command = Command::new("sh");
        command.args(["-c", "trap '' TERM; sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn stub harness");
        let process_identity = process.identity().clone();
        let state_path = directory.0.join("active-runs.json");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record("arun_recovery", process_identity, &workspace)
            .expect("persist active run");
        drop(store);
        let restarted_store =
            ActiveRunStore::open(&state_path).expect("load state after runner restart");

        let recovered = recover_active_runs(
            &restarted_store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover active run");
        assert_eq!(recovered, ["arun_recovery"]);
        assert!(restarted_store.records().is_empty());
        assert!(!workspace.exists());
        process
            .wait_timeout(Duration::from_secs(1), Duration::from_millis(50))
            .expect("reap terminated test harness");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_preserves_state_and_workspace_when_termination_is_unconfirmed() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("user-file"), "keep").expect("write workspace file");

        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn test harness");
        let store = ActiveRunStore::open(directory.0.join("active-runs.json"))
            .expect("open active-run state");
        store
            .record(
                "arun_unconfirmed_termination",
                process.identity().clone(),
                &workspace,
            )
            .expect("persist active run");

        let error = recover_active_runs_with(
            &store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
            |_, _| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "orphan process group did not stop after SIGKILL",
                ))
            },
        )
        .expect_err("unconfirmed termination must stop recovery");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(store.records().len(), 1);
        assert!(workspace.join("user-file").is_file());
        assert!(process.identity().matches_live_process());
        process
            .terminate(Duration::from_millis(50))
            .expect("stop test harness after recovery check");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_terminates_group_members_after_leader_exits() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        let state_path = directory.0.join("active-runs.json");
        let ready_path = directory.0.join("descendant.pid");

        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 300 >/dev/null 2>&1 & echo $! > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"; exec sleep 300",
                "stub-harness",
            ])
            .arg(&ready_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let leader = SupervisedProcess::spawn(&mut command).expect("spawn stub harness group");
        let process_identity = leader.identity().clone();
        let leader_pid = process_identity.process_id();
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record("arun_orphan_group", process_identity, &workspace)
            .expect("persist active run");

        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready_path.exists(), "harness did not start its descendant");
        let descendant = fs::read_to_string(&ready_path)
            .expect("read descendant PID")
            .trim()
            .parse::<u32>()
            .expect("valid descendant PID");
        assert!(linux_process_state(descendant).is_some_and(|state| !matches!(state, 'Z' | 'X')));

        let leader_kill = Command::new("sh")
            .args([
                "-c",
                "kill -KILL \"$1\"",
                "kill-leader",
                &leader_pid.to_string(),
            ])
            .status()
            .expect("kill process-group leader");
        assert!(leader_kill.success());
        leader.wait().expect("reap process-group leader");
        assert!(linux_process_state(leader_pid).is_none());

        drop(store);
        let restarted_store =
            ActiveRunStore::open(&state_path).expect("load active state after leader exit");
        let recovered = recover_active_runs(
            &restarted_store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover surviving process-group member");

        assert_eq!(recovered, ["arun_orphan_group"]);
        assert!(restarted_store.records().is_empty());
        assert!(!workspace.exists());
        let stopped_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while linux_process_state(descendant).is_some_and(|state| !matches!(state, 'Z' | 'X'))
            && std::time::Instant::now() < stopped_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(linux_process_state(descendant).is_none_or(|state| matches!(state, 'Z' | 'X')));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recovery_terminates_macOS_group_members_after_leader_exits() {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        let state_path = directory.0.join("active-runs.json");
        let ready_path = directory.0.join("descendant.pid");

        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 300 >/dev/null 2>&1 & echo $! > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"; exec sleep 300",
                "stub-harness",
            ])
            .arg(&ready_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut leader = command.spawn().expect("spawn stub harness group");
        let _group_cleanup = TestProcessGroup(leader.id());
        let identity = ProcessIdentity::for_test_child(&leader);
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record("arun_orphan_group", identity, &workspace)
            .expect("persist active run");

        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready_path.exists(), "harness did not start its descendant");
        let descendant = fs::read_to_string(&ready_path)
            .expect("read descendant PID")
            .trim()
            .parse::<u32>()
            .expect("valid descendant PID");
        assert!(macos_process_exists(descendant));

        leader.kill().expect("kill process-group leader");
        leader.wait().expect("reap process-group leader");
        assert!(
            macos_process_exists(descendant),
            "descendant exited with its process-group leader"
        );

        drop(store);
        let restarted_store =
            ActiveRunStore::open(&state_path).expect("load active state after leader exit");
        let recovered = recover_active_runs(
            &restarted_store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover surviving process-group member");

        assert_eq!(recovered, ["arun_orphan_group"]);
        assert!(restarted_store.records().is_empty());
        assert!(!workspace.exists());
        let stopped_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while macos_process_exists(descendant) && std::time::Instant::now() < stopped_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!macos_process_exists(descendant));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recovery_preserves_workspace_when_process_generation_is_uncertain() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("user-file"), "keep").expect("write workspace file");
        let state_path = directory.0.join("active-runs.json");

        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn test harness");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record(
                "arun_uncertain_identity",
                process.identity().clone(),
                &workspace,
            )
            .expect("persist active run");
        drop(store);

        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read active-run state"))
                .expect("parse active-run state");
        let start_time = state["runs"]["arun_uncertain_identity"]["process"]["start_time_ticks"]
            .as_u64()
            .expect("recorded process start time");
        state["runs"]["arun_uncertain_identity"]["process"]["start_time_ticks"] =
            serde_json::Value::from(start_time + 1);
        fs::write(
            &state_path,
            serde_json::to_vec(&state).expect("serialize stale active-run state"),
        )
        .expect("write stale active-run state");

        let restarted_store = ActiveRunStore::open(&state_path).expect("load stale state");
        let error = recover_active_runs(
            &restarted_store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect_err("preserve state when the process generation is uncertain");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(restarted_store.records().len(), 1);
        assert!(workspace.join("user-file").is_file());
        assert!(process.identity().matches_live_process());
        process
            .terminate(Duration::from_millis(50))
            .expect("stop test harness after stale identity check");
    }

    #[cfg(unix)]
    #[test]
    fn active_state_survives_reopen_and_is_removed_atomically() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn stub harness");
        let store_path = directory.0.join("active-runs.json");
        let store = ActiveRunStore::open(&store_path).expect("open active-run state");
        store
            .record("arun_persist", process.identity().clone(), &workspace)
            .expect("persist active run");
        drop(store);

        let reopened = ActiveRunStore::open(&store_path).expect("reopen active-run state");
        assert_eq!(reopened.records().len(), 1);
        reopened.remove("arun_persist").expect("remove active run");
        assert!(reopened.records().is_empty());
        process
            .terminate(Duration::from_millis(50))
            .expect("stop test harness");
    }

    #[test]
    fn recovery_cleans_workspace_abandoned_before_harness_spawn() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("partial-checkout"), "in progress")
            .expect("write partial checkout");
        let state_path = directory.0.join("active-runs.json");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_workspace("arun_preparing", &workspace)
            .expect("persist workspace before checkout");
        drop(store);

        let restarted_store =
            ActiveRunStore::open(&state_path).expect("load state after runner restart");
        let recovered = recover_active_runs(
            &restarted_store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover workspace without a spawned harness");
        assert_eq!(recovered, ["arun_preparing"]);
        assert!(restarted_store.records().is_empty());
        assert!(!workspace.exists());
    }

    #[test]
    fn recovery_leaves_workspaces_outside_configured_roots_untouched() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("user-file"), "keep").expect("write workspace file");
        let state_path = directory.0.join("active-runs.json");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_workspace("arun_outside_root", &workspace)
            .expect("persist workspace");

        let configured_root = directory.0.join("configured-workspaces");
        fs::create_dir_all(&configured_root).expect("create configured workspace root");
        let result = recover_active_runs(
            &store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&configured_root),
        );

        assert_eq!(
            result
                .expect_err("reject workspace outside configured roots")
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(workspace.join("user-file").is_file());
        assert_eq!(store.records().len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphan_harness_fixture_process() {
        let Some(state_path) = std::env::var_os("TINES_RUNNER_RECOVERY_STATE") else {
            return;
        };
        let ready_path =
            std::env::var_os("TINES_RUNNER_RECOVERY_READY").expect("fixture ready path is set");
        let workspace = std::env::var_os("TINES_RUNNER_RECOVERY_WORKSPACE")
            .expect("fixture workspace path is set");
        let workspace = PathBuf::from(workspace);
        fs::create_dir_all(&workspace).expect("create fixture workspace");
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 300"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn orphan fixture");
        let store = ActiveRunStore::open(state_path).expect("open fixture active state");
        store
            .record("arun_killed_runner", process.identity().clone(), &workspace)
            .expect("persist fixture active run");
        fs::write(ready_path, "ready").expect("signal fixture readiness");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restart_after_killed_runner_terminates_its_orphan_harness() {
        let directory = TestDirectory::new();
        let state_path = directory.0.join("active-runs.json");
        let ready_path = directory.0.join("runner-ready");
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        let mut runner = Command::new(std::env::current_exe().expect("current test binary"));
        runner
            .args([
                "--exact",
                "recovery::tests::orphan_harness_fixture_process",
                "--nocapture",
            ])
            .env("TINES_RUNNER_RECOVERY_STATE", &state_path)
            .env("TINES_RUNNER_RECOVERY_READY", &ready_path)
            .env("TINES_RUNNER_RECOVERY_WORKSPACE", &workspace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut runner = runner.spawn().expect("start fixture runner process");

        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready_path.exists() {
            let _ = runner.kill();
            let _ = runner.wait();
            panic!("fixture runner did not persist its harness state");
        }

        runner.kill().expect("kill fixture runner");
        runner.wait().expect("reap fixture runner");
        let store = ActiveRunStore::open(&state_path).expect("load state after runner crash");
        let orphan = store
            .records()
            .into_iter()
            .next()
            .expect("runner persisted active harness");
        let process_id = orphan
            .process
            .as_ref()
            .expect("fixture has process identity")
            .process_id();

        let recovered = recover_active_runs(
            &store,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover after runner crash");
        assert_eq!(recovered, ["arun_killed_runner"]);
        assert!(!workspace.exists());
        assert!(store.records().is_empty());

        let stopped_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while linux_process_state(process_id).is_some_and(|state| !matches!(state, 'Z' | 'X'))
            && std::time::Instant::now() < stopped_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(linux_process_state(process_id).is_none_or(|state| matches!(state, 'Z' | 'X')));
    }

    #[cfg(target_os = "linux")]
    fn linux_process_state(process_id: u32) -> Option<char> {
        let stat = fs::read_to_string(format!("/proc/{process_id}/stat")).ok()?;
        stat.rsplit_once(") ")?.1.chars().next()
    }

    #[cfg(target_os = "macos")]
    struct TestProcessGroup(u32);

    #[cfg(target_os = "macos")]
    impl Drop for TestProcessGroup {
        fn drop(&mut self) {
            unsafe {
                let _ = kill(-(self.0 as i32), 9);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_process_exists(process_id: u32) -> bool {
        unsafe { kill(process_id as i32, 0) == 0 }
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn kill(process_id: i32, signal: i32) -> i32;
    }
}
