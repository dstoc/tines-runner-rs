//! Persistent active-run state and crash recovery.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::WorkspaceRetention;
use crate::process::{EXECUTOR_TRANSPORT_TERMINATION_GRACE, ProcessIdentity};
use crate::retention;

const STATE_VERSION: u8 = 3;
const ORPHAN_TERMINATION_GRACE: Duration = EXECUTOR_TRANSPORT_TERMINATION_GRACE;

/// The durable local information needed to recover one active assignment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveRunRecord {
    pub run_id: String,
    /// The daemon-owned executor transport process group. `process` is
    /// accepted when reading version-1 state written by earlier runners.
    #[serde(default, alias = "process")]
    pub transport: Option<ProcessIdentity>,
    /// A workspace written by an older daemon. New runs leave workspace
    /// creation and recovery cleanup to the executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
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
        validate_state_location(&path)?;
        let runs = read_state(&path)?;

        Ok(Self {
            inner: Arc::new(StoreInner {
                path,
                runs: Mutex::new(runs),
            }),
        })
    }

    /// Import state from the credentials-adjacent path used by older versions.
    /// The legacy file is only read; a marker in the configured state directory
    /// prevents stale records from being re-imported after settlement.
    pub fn import_legacy_state(&self, legacy_path: &Path) -> io::Result<usize> {
        if legacy_path == self.inner.path {
            return Ok(0);
        }
        let marker = migration_marker(&self.inner.path);
        match fs::metadata(&marker) {
            Ok(_) => return Ok(0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(state_path_error(
                    "could not inspect active-run migration marker for",
                    &self.inner.path,
                    error,
                ));
            }
        }

        let state_exists = self
            .inner
            .path
            .try_exists()
            .map_err(|error| state_path_error("could not inspect", &self.inner.path, error))?;
        let mut runs = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let imported = if state_exists || !runs.is_empty() {
            0
        } else {
            let legacy_runs = read_state(legacy_path)?;
            if legacy_runs.is_empty() {
                0
            } else {
                write_state(&self.inner.path, &legacy_runs)?;
                *runs = legacy_runs.clone();
                legacy_runs.len()
            }
        };
        write_migration_marker(&marker, &self.inner.path)?;
        Ok(imported)
    }

    /// Persist the executor transport identity after it starts and before
    /// waiting for its protocol stream.
    pub fn record_transport(
        &self,
        run_id: impl Into<String>,
        transport: ProcessIdentity,
        legacy_workspace: Option<&Path>,
    ) -> io::Result<()> {
        self.update_record(run_id.into(), Some(transport), legacy_workspace)
    }

    fn update_record(
        &self,
        run_id: String,
        process: Option<ProcessIdentity>,
        workspace: Option<&Path>,
    ) -> io::Result<()> {
        let workspace = workspace.map(fs::canonicalize).transpose()?;
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
                transport: process,
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

/// Stop orphaned transports, clean legacy daemon workspaces, and clear records.
///
/// The caller must run this before the first poll. The following empty
/// `owned_runs` list lets Tines reconcile any still-active server runs as
/// interrupted.
pub fn recover_active_runs(
    store: &ActiveRunStore,
    retention: &WorkspaceRetention,
    workspace_roots: &[PathBuf],
) -> io::Result<Vec<String>> {
    recover_active_runs_with(store, retention, workspace_roots, |transport, grace| {
        transport.terminate_if_matches(grace)
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
        if let Some(workspace) = &record.workspace {
            validate_workspace_root(workspace, workspace_roots)?;
        }
        let terminated = record
            .transport
            .as_ref()
            .map(|transport| terminate(transport, ORPHAN_TERMINATION_GRACE))
            .transpose()?
            .unwrap_or(false);
        if terminated {
            tracing::warn!(
                run_id = %record.run_id,
                process_id = record.transport.as_ref().map(ProcessIdentity::process_id),
                "terminated an orphaned executor transport during startup recovery"
            );
        } else {
            tracing::info!(
                run_id = %record.run_id,
                process_id = record.transport.as_ref().map(ProcessIdentity::process_id),
                "no matching orphan executor transport remained; leaving any reused PID untouched"
            );
        }

        if let Some(workspace) = &record.workspace {
            retention::settle_recovered_workspace(workspace, retention, &record.run_id)
                .map_err(io::Error::other)?;
            if let Some(parent) = workspace.parent() {
                retention::prune_retained(parent, retention).map_err(io::Error::other)?;
            }
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
    fs::create_dir_all(parent).map_err(|error| {
        state_path_error("could not create configured runner state path", path, error)
    })?;
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
    result.map_err(|error| state_path_error("could not write", path, error))
}

fn read_state(path: &Path) -> io::Result<BTreeMap<String, ActiveRunRecord>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(state_path_error("could not read", path, error)),
    };
    let state: ActiveRunFile = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid active-run state file {}: {error}", path.display()),
        )
    })?;
    if !matches!(state.version, 1 | 2 | STATE_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported active-run state version {} in {}",
                state.version,
                path.display()
            ),
        ));
    }
    for (run_id, record) in &state.runs {
        if run_id != &record.run_id
            || run_id.is_empty()
            || record
                .workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.is_absolute())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "active-run state {} contains an invalid run ID or workspace path",
                    path.display()
                ),
            ));
        }
    }
    Ok(state.runs)
}

fn migration_marker(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("active-runs");
    path.with_file_name(format!(".{name}.legacy-imported"))
}

fn write_migration_marker(marker: &Path, state_path: &Path) -> io::Result<()> {
    let parent = marker.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".active-run-migration-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = write_temporary_state(&temporary, b"imported\n")
        .and_then(|()| fs::rename(&temporary, marker))
        .and_then(|()| sync_directory(parent));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| {
        state_path_error(
            "could not persist active-run migration marker for",
            state_path,
            error,
        )
    })
}

fn validate_state_location(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        state_path_error("configured runner state path is not writable", path, error)
    })?;
    let metadata = fs::metadata(parent).map_err(|error| {
        state_path_error(
            "could not inspect configured runner state path",
            path,
            error,
        )
    })?;
    if !metadata.is_dir() {
        return Err(state_path_error(
            "configured runner state path is not a directory",
            path,
            io::Error::new(
                io::ErrorKind::NotADirectory,
                "parent path is not a directory",
            ),
        ));
    }

    let token = uuid::Uuid::new_v4();
    let temporary = parent.join(format!(".active-runs-{token}.probe.tmp"));
    let renamed = parent.join(format!(".active-runs-{token}.probe"));
    let result = write_temporary_state(&temporary, b"state path check")
        .and_then(|()| fs::rename(&temporary, &renamed))
        .and_then(|()| sync_directory(parent))
        .and_then(|()| fs::remove_file(&renamed))
        .and_then(|()| sync_directory(parent));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(&renamed);
    }
    result.map_err(|error| {
        state_path_error("configured runner state path is not writable", path, error)
    })
}

fn state_path_error(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
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

    #[test]
    fn invalid_state_location_reports_the_configured_state_path() {
        let directory = TestDirectory::new();
        let not_a_directory = directory.0.join("not-a-directory");
        fs::write(&not_a_directory, "occupied").expect("create state path obstruction");
        let state_path = not_a_directory.join("active-runs.json");

        let error = ActiveRunStore::open(&state_path)
            .expect_err("a file cannot be used as the state directory");
        let message = error.to_string();
        assert!(
            message.contains("configured runner state path"),
            "{message}"
        );
        assert!(
            message.contains(&state_path.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn legacy_state_is_imported_once_without_writing_to_its_directory() {
        let directory = TestDirectory::new();
        let credential_dir = directory.0.join("external-credentials");
        fs::create_dir_all(&credential_dir).expect("create legacy credentials directory");
        let legacy_path = credential_dir.join("active-runs.json");
        let legacy_store = ActiveRunStore::open(&legacy_path).expect("open legacy state file");
        legacy_store
            .update_record("arun_legacy".to_owned(), None, None)
            .expect("write legacy active run");
        drop(legacy_store);
        let original_legacy_state = fs::read(&legacy_path).expect("read legacy state bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&legacy_path, fs::Permissions::from_mode(0o400))
                .expect("make legacy state read-only");
            fs::set_permissions(&credential_dir, fs::Permissions::from_mode(0o500))
                .expect("make legacy credentials directory read-only");
        }

        let state_dir = directory.0.join("writable-state");
        let state_path = state_dir.join("active-runs.json");
        let store = ActiveRunStore::open(&state_path).expect("open configured writable state");
        assert_eq!(
            store
                .import_legacy_state(&legacy_path)
                .expect("import old recovery records"),
            1
        );
        assert_eq!(store.path(), state_path);
        assert_eq!(
            store
                .records()
                .iter()
                .map(|record| record.run_id.as_str())
                .collect::<Vec<_>>(),
            ["arun_legacy"]
        );
        assert_eq!(
            fs::read(&legacy_path).expect("legacy source remains readable"),
            original_legacy_state,
            "the old credentials-adjacent state is read but never changed"
        );
        assert!(state_path.is_file(), "new state belongs under state_dir");

        store.remove("arun_legacy").expect("settle imported run");
        drop(store);
        let reopened = ActiveRunStore::open(&state_path).expect("reopen configured state");
        assert_eq!(
            reopened
                .import_legacy_state(&legacy_path)
                .expect("do not import stale legacy records again"),
            0
        );
        assert!(reopened.records().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_state_location_fails_before_polling_with_its_path() {
        let state_path = PathBuf::from(format!(
            "/proc/tines-runner-state-{}-{}/active-runs.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let error = ActiveRunStore::open(&state_path)
            .expect_err("procfs does not allow creating runner state directories");
        let message = error.to_string();
        assert!(
            message.contains("configured runner state path"),
            "{message}"
        );
        assert!(
            message.contains(&state_path.display().to_string()),
            "{message}"
        );
    }

    fn retention(mode: RetentionMode) -> WorkspaceRetention {
        WorkspaceRetention {
            mode,
            max_age: Duration::from_secs(3600),
            max_count: 10,
        }
    }

    #[test]
    fn version_one_process_identity_loads_as_transport_and_rewrites_as_version_two() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("legacy-workspace");
        fs::create_dir_all(&workspace).expect("create legacy workspace");
        let state_path = directory.0.join("active-runs.json");
        let legacy = serde_json::json!({
            "version": 1,
            "runs": {
                "arun_legacy": {
                    "run_id": "arun_legacy",
                    "process": {
                        "process_id": 42,
                        "process_group_id": 42,
                        "boot_id": "legacy-boot",
                        "start_time_ticks": 123
                    },
                    "workspace": workspace
                }
            }
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&legacy).expect("encode version-one state"),
        )
        .expect("write version-one state");

        let store = ActiveRunStore::open(&state_path).expect("load version-one active state");
        let record = store.records().pop().expect("load legacy record");
        assert_eq!(record.transport.as_ref().unwrap().process_id(), 42);
        store
            .record_transport(
                "arun_legacy",
                record.transport.expect("legacy transport identity"),
                Some(&workspace),
            )
            .expect("write current active state");
        let current: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read current state"))
                .expect("parse current state");
        assert_eq!(current["version"], 3);
        assert!(current["runs"]["arun_legacy"]["transport"].is_object());
        assert!(current["runs"]["arun_legacy"].get("process").is_none());
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
            .record_transport("arun_recovery", process_identity, Some(&workspace))
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

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_does_not_kill_unrelated_process_when_pid_generation_changed() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join(format!("run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&workspace).expect("create workspace");
        let state_path = directory.0.join("active-runs.json");
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn unrelated process");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_transport(
                "arun_reused_pid",
                process.identity().clone(),
                Some(&workspace),
            )
            .expect("persist transport identity");
        drop(store);

        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read active-run state"))
                .expect("parse active-run state");
        let recorded_start = state["runs"]["arun_reused_pid"]["transport"]["start_time_ticks"]
            .as_u64()
            .expect("recorded process generation");
        state["runs"]["arun_reused_pid"]["transport"]["start_time_ticks"] =
            serde_json::Value::from(recorded_start + 1);
        fs::write(
            &state_path,
            serde_json::to_vec(&state).expect("encode mismatched process identity"),
        )
        .expect("write mismatched process identity");

        let restarted = ActiveRunStore::open(&state_path).expect("load state after daemon restart");
        let recovered = recover_active_runs(
            &restarted,
            &retention(RetentionMode::Never),
            std::slice::from_ref(&directory.0),
        )
        .expect("recover without signaling a reused PID");
        assert_eq!(recovered, ["arun_reused_pid"]);
        assert!(restarted.records().is_empty());
        assert!(!workspace.exists());
        assert!(
            process.identity().matches_live_process(),
            "the unrelated process generation remains alive"
        );
        process
            .terminate(Duration::from_millis(50))
            .expect("stop unrelated test process");
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
            .record_transport(
                "arun_unconfirmed_termination",
                process.identity().clone(),
                Some(&workspace),
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
        let leader =
            SupervisedProcess::spawn(&mut command).expect("spawn stub executor transport group");
        let process_identity = leader.identity().clone();
        let leader_pid = process_identity.process_id();
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_transport("arun_orphan_group", process_identity, Some(&workspace))
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
        let mut leader = command
            .spawn()
            .expect("spawn stub executor transport group");
        let _group_cleanup = TestProcessGroup(leader.id());
        let identity = ProcessIdentity::for_test_child(&leader);
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_transport("arun_orphan_group", identity, Some(&workspace))
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
            .record_transport(
                "arun_uncertain_identity",
                process.identity().clone(),
                Some(&workspace),
            )
            .expect("persist active run");
        drop(store);

        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read active-run state"))
                .expect("parse active-run state");
        let start_time = state["runs"]["arun_uncertain_identity"]["transport"]["start_time_ticks"]
            .as_u64()
            .expect("recorded process start time");
        state["runs"]["arun_uncertain_identity"]["transport"]["start_time_ticks"] =
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
            .record_transport("arun_persist", process.identity().clone(), Some(&workspace))
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

    #[cfg(unix)]
    #[test]
    fn new_active_records_track_transport_without_a_daemon_workspace() {
        let directory = TestDirectory::new();
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let process = SupervisedProcess::spawn(&mut command).expect("spawn executor transport");
        let state_path = directory.0.join("active-runs.json");
        let store = ActiveRunStore::open(&state_path).expect("open active-run state");
        store
            .record_transport("arun_executor_workspace", process.identity().clone(), None)
            .expect("persist transport without a daemon workspace");

        let raw_state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read active state"))
                .expect("parse active state");
        assert!(
            raw_state["runs"]["arun_executor_workspace"]
                .get("workspace")
                .is_none()
        );

        let recovered = recover_active_runs(&store, &retention(RetentionMode::Never), &[])
            .expect("recover executor transport without a daemon workspace");
        assert_eq!(recovered, ["arun_executor_workspace"]);
        assert!(store.records().is_empty());
        process
            .wait_timeout(Duration::from_secs(1), Duration::from_millis(50))
            .expect("reap recovered executor transport");
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
            .update_record("arun_preparing".to_owned(), None, Some(&workspace))
            .expect("persist legacy workspace before checkout");
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
            .update_record("arun_outside_root".to_owned(), None, Some(&workspace))
            .expect("persist legacy workspace");

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
            .record_transport(
                "arun_killed_runner",
                process.identity().clone(),
                Some(&workspace),
            )
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
            .transport
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
