//! Retention markers and pruning for settled assignment workspaces.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{RetentionMode, WorkspaceRetention};
use crate::protocol::FinishStatus;

const MARKER_NAME: &str = ".tines-runner-retained.json";

/// Metadata that identifies a settled workspace eligible for pruning.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RetainedWorkspaceMarker {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_ref: Option<String>,
    pub terminal_status: FinishStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub retained_at: u64,
}

/// Apply the configured retention policy after Tines accepts a terminal report.
///
/// A workspace selected for retention receives its marker before pruning runs.
/// A workspace that is not retained is removed as the known, settled run
/// directory. Pruning only considers direct child directories with valid
/// marker files.
pub fn settle_workspace(
    workspace: &Path,
    retention: &WorkspaceRetention,
    run_id: &str,
    issue_ref: Option<String>,
    status: FinishStatus,
    error: Option<&str>,
) -> Result<(), RetentionError> {
    let parent = workspace.parent().ok_or(RetentionError::MissingParent)?;

    if should_retain(retention.mode, status) {
        write_marker(
            workspace,
            &RetainedWorkspaceMarker {
                run_id: run_id.to_owned(),
                issue_ref,
                terminal_status: status,
                error: error.map(str::to_owned),
                retained_at: unix_now(),
            },
        )?;
    } else {
        remove_workspace(workspace)?;
    }

    prune_retained(parent, retention)?;
    Ok(())
}

/// Return whether a terminal outcome is selected by a retention mode.
pub fn should_retain(mode: RetentionMode, status: FinishStatus) -> bool {
    match mode {
        RetentionMode::Never => false,
        RetentionMode::Failed => status == FinishStatus::Failed,
        RetentionMode::Always => true,
    }
}

/// Prune expired and over-limit retained workspaces below `parent`.
pub fn prune_retained(parent: &Path, retention: &WorkspaceRetention) -> Result<(), RetentionError> {
    prune_retained_at(parent, retention, unix_now())
}

/// Prune retained workspaces below each configured workspace parent.
pub fn prune_retained_roots(
    parents: impl IntoIterator<Item = impl AsRef<Path>>,
    retention: &WorkspaceRetention,
) -> Result<(), RetentionError> {
    let now = unix_now();
    let mut visited = Vec::new();
    for parent in parents {
        let parent = parent.as_ref();
        if visited.iter().any(|visited| visited == parent) {
            continue;
        }
        visited.push(parent.to_path_buf());
        prune_retained_at(parent, retention, now)?;
    }
    Ok(())
}

fn prune_retained_at(
    parent: &Path,
    retention: &WorkspaceRetention,
    now: u64,
) -> Result<(), RetentionError> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(RetentionError::Io {
                operation: "read workspace parent",
                source,
            });
        }
    };

    let mut marked = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RetentionError::Io {
            operation: "read workspace directory entry",
            source,
        })?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|source| RetentionError::Io {
                operation: "inspect workspace directory entry",
                source,
            })?
            .is_dir()
        {
            continue;
        }
        let Some(marker) = read_valid_marker(&path)? else {
            continue;
        };
        marked.push((path, marker.retained_at));
    }

    marked.sort_by(|(left_path, left_time), (right_path, right_time)| {
        right_time
            .cmp(left_time)
            .then_with(|| left_path.cmp(right_path))
    });

    let max_age = retention.max_age.as_secs();
    for (path, retained_at) in &marked {
        if now.saturating_sub(*retained_at) >= max_age {
            remove_marked_workspace(path)?;
        }
    }

    let mut kept = 0usize;
    for (path, retained_at) in marked {
        if now.saturating_sub(retained_at) >= max_age {
            continue;
        }
        if kept < retention.max_count {
            kept += 1;
        } else {
            remove_marked_workspace(&path)?;
        }
    }

    Ok(())
}

fn read_valid_marker(workspace: &Path) -> Result<Option<RetainedWorkspaceMarker>, RetentionError> {
    let marker_path = workspace.join(MARKER_NAME);
    let metadata = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RetentionError::Io {
                operation: "inspect retention marker",
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }

    let marker_file = match File::open(&marker_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RetentionError::Io {
                operation: "open retention marker",
                source,
            });
        }
    };
    let marker: RetainedWorkspaceMarker =
        match serde_json::from_reader::<_, RetainedWorkspaceMarker>(marker_file) {
            Ok(marker) if !marker.run_id.trim().is_empty() => marker,
            Ok(_) | Err(_) => return Ok(None),
        };
    Ok(Some(marker))
}

fn write_marker(workspace: &Path, marker: &RetainedWorkspaceMarker) -> Result<(), RetentionError> {
    let marker_path = workspace.join(MARKER_NAME);
    let temporary_path = workspace.join(format!("{MARKER_NAME}.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|source| RetentionError::Io {
                operation: "create retention marker",
                source,
            })?;
        serde_json::to_writer(&mut file, marker).map_err(RetentionError::SerializeMarker)?;
        file.write_all(b"\n").map_err(|source| RetentionError::Io {
            operation: "write retention marker",
            source,
        })?;
        file.sync_all().map_err(|source| RetentionError::Io {
            operation: "sync retention marker",
            source,
        })?;
        fs::rename(&temporary_path, &marker_path).map_err(|source| RetentionError::Io {
            operation: "publish retention marker",
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result
}

fn remove_workspace(workspace: &Path) -> Result<(), RetentionError> {
    match fs::remove_dir_all(workspace) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RetentionError::Io {
            operation: "remove settled workspace",
            source,
        }),
    }
}

fn remove_marked_workspace(workspace: &Path) -> Result<(), RetentionError> {
    if read_valid_marker(workspace)?.is_none() {
        return Ok(());
    }
    remove_workspace(workspace)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// An error while writing or pruning retained workspaces.
#[derive(Debug)]
pub enum RetentionError {
    MissingParent,
    Io {
        operation: &'static str,
        source: io::Error,
    },
    SerializeMarker(serde_json::Error),
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParent => f.write_str("workspace path has no parent directory"),
            Self::Io { operation, source } => write!(f, "could not {operation}: {source}"),
            Self::SerializeMarker(source) => {
                write!(f, "could not serialize retention marker: {source}")
            }
        }
    }
}

impl Error for RetentionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingParent => None,
            Self::Io { source, .. } => Some(source),
            Self::SerializeMarker(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tines-runner-retention-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn workspace(&self, name: &str) -> PathBuf {
            let workspace = self.0.join(name);
            fs::create_dir(&workspace).expect("create workspace");
            fs::write(workspace.join("prompt.md"), "debug data").expect("write workspace data");
            workspace
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
            max_age: Duration::from_secs(60 * 60),
            max_count: 20,
        }
    }

    #[test]
    fn retention_modes_keep_only_selected_terminal_outcomes() {
        for (mode, status, expected) in [
            (RetentionMode::Never, FinishStatus::Completed, false),
            (RetentionMode::Never, FinishStatus::Failed, false),
            (RetentionMode::Failed, FinishStatus::Completed, false),
            (RetentionMode::Failed, FinishStatus::Failed, true),
            (RetentionMode::Always, FinishStatus::Completed, true),
            (RetentionMode::Always, FinishStatus::Failed, true),
        ] {
            let directory = TestDirectory::new();
            let workspace = directory.workspace("run");
            settle_workspace(
                &workspace,
                &retention(mode),
                "run_123",
                Some("Tines/19".to_owned()),
                status,
                (status == FinishStatus::Failed).then_some("Codex exited with code 1"),
            )
            .expect("settle workspace");

            assert_eq!(
                workspace.exists(),
                expected,
                "mode {mode:?}, status {status:?}"
            );
            if expected {
                let marker: RetainedWorkspaceMarker = serde_json::from_slice(
                    &fs::read(workspace.join(MARKER_NAME)).expect("read marker"),
                )
                .expect("parse marker");
                assert_eq!(marker.run_id, "run_123");
                assert_eq!(marker.issue_ref.as_deref(), Some("Tines/19"));
                assert_eq!(marker.terminal_status, status);
                assert_eq!(
                    marker.error.as_deref(),
                    (status == FinishStatus::Failed).then_some("Codex exited with code 1")
                );
                assert!(marker.retained_at > 0);
            }
        }
    }

    #[test]
    fn pruning_never_deletes_unmarked_or_invalid_marker_directories() {
        let directory = TestDirectory::new();
        let unmarked = directory.workspace("unmarked");
        let invalid = directory.workspace("invalid");
        fs::write(invalid.join(MARKER_NAME), b"not json").expect("write invalid marker");
        let expired = directory.workspace("expired");
        write_marker(
            &expired,
            &RetainedWorkspaceMarker {
                run_id: "run_old".to_owned(),
                issue_ref: None,
                terminal_status: FinishStatus::Failed,
                error: Some("old failure".to_owned()),
                retained_at: 1,
            },
        )
        .expect("write expired marker");

        prune_retained_at(
            &directory.0,
            &WorkspaceRetention {
                mode: RetentionMode::Always,
                max_age: Duration::from_secs(10),
                max_count: 20,
            },
            100,
        )
        .expect("prune expired workspace");

        assert!(unmarked.exists());
        assert!(invalid.exists());
        assert!(!expired.exists());
    }

    #[test]
    fn startup_pruning_covers_override_roots_and_keeps_unmarked_directories() {
        let directory = TestDirectory::new();
        let default_root = directory.0.join("default");
        let override_root = directory.0.join("override");
        fs::create_dir_all(&default_root).expect("create default workspace root");
        fs::create_dir_all(&override_root).expect("create override workspace root");
        let expired = override_root.join("expired");
        fs::create_dir(&expired).expect("create expired workspace");
        write_marker(
            &expired,
            &RetainedWorkspaceMarker {
                run_id: "run_old".to_owned(),
                issue_ref: Some("Tines/19".to_owned()),
                terminal_status: FinishStatus::Failed,
                error: Some("old failure".to_owned()),
                retained_at: 1,
            },
        )
        .expect("write expired marker");
        let unmarked = override_root.join("unmarked");
        fs::create_dir(&unmarked).expect("create unmarked workspace");

        prune_retained_roots(
            [&default_root, &override_root, &override_root],
            &WorkspaceRetention {
                mode: RetentionMode::Always,
                max_age: Duration::from_secs(10),
                max_count: 20,
            },
        )
        .expect("prune all configured workspace roots");

        assert!(!expired.exists());
        assert!(unmarked.exists());
    }

    #[test]
    fn pruning_enforces_maximum_retained_count_by_timestamp() {
        let directory = TestDirectory::new();
        let older = directory.workspace("older");
        let newer = directory.workspace("newer");
        for (workspace, run_id, retained_at) in [(&older, "run_old", 80), (&newer, "run_new", 90)] {
            write_marker(
                workspace,
                &RetainedWorkspaceMarker {
                    run_id: run_id.to_owned(),
                    issue_ref: None,
                    terminal_status: FinishStatus::Completed,
                    error: None,
                    retained_at,
                },
            )
            .expect("write marker");
        }

        prune_retained_at(
            &directory.0,
            &WorkspaceRetention {
                mode: RetentionMode::Always,
                max_age: Duration::from_secs(100),
                max_count: 1,
            },
            100,
        )
        .expect("prune to retained count");

        assert!(!older.exists());
        assert!(newer.exists());
    }
}
