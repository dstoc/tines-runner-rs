//! Per-assignment workspace materialization and retention.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::protocol::{RunnerAssignment, RunnerAssignmentEnv};

const SKILLS_PATH: &str = ".agents/skills";

/// A unique workspace and the assignment environment kept in memory for launch.
#[derive(Clone)]
pub struct MaterializedWorkspace {
    path: PathBuf,
    environment: LaunchEnvironment,
}

impl MaterializedWorkspace {
    /// Create one new workspace below `parent` and write the cold-run inputs.
    pub fn create(
        parent: impl AsRef<Path>,
        assignment: &RunnerAssignment,
        api_url: &Url,
    ) -> Result<Self, WorkspaceError> {
        let environment = LaunchEnvironment::new(
            assignment.run_key.clone(),
            api_url.as_str().trim_end_matches('/').to_owned(),
            &assignment.env,
        )?;
        let parent = parent.as_ref();
        fs::create_dir_all(parent).map_err(|source| WorkspaceError::Io {
            operation: "create workspace parent",
            source,
        })?;
        let path = create_unique_workspace(parent)?;
        let result = materialize_contents(&path, assignment);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&path);
            return Err(error);
        }

        Ok(Self { path, environment })
    }

    /// The unique directory for this assignment.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Assignment variables plus the canonical Tines API variables.
    pub fn environment(&self) -> &LaunchEnvironment {
        &self.environment
    }
}

impl fmt::Debug for MaterializedWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaterializedWorkspace")
            .field("path", &self.path)
            .field("environment", &self.environment)
            .finish()
    }
}

/// Environment metadata for a later harness process launch.
///
/// Values are intentionally omitted from `Debug`; environment entries can
/// contain secrets and the run key is always secret.
#[derive(Clone)]
pub struct LaunchEnvironment {
    run_key: String,
    api_url: String,
    variables: BTreeMap<String, RunnerAssignmentEnv>,
}

impl LaunchEnvironment {
    fn new(
        run_key: String,
        api_url: String,
        entries: &[RunnerAssignmentEnv],
    ) -> Result<Self, WorkspaceError> {
        let mut variables = BTreeMap::new();
        for entry in entries {
            if !valid_environment_name(&entry.name) || entry.value.contains('\0') {
                return Err(WorkspaceError::InvalidEnvironment);
            }
            if variables
                .insert(entry.name.clone(), entry.clone())
                .is_some()
            {
                return Err(WorkspaceError::DuplicateEnvironmentName);
            }
        }
        Ok(Self {
            run_key,
            api_url,
            variables,
        })
    }

    /// Names delivered by the assignment, for safe launch diagnostics.
    pub fn variable_names(&self) -> impl Iterator<Item = &str> {
        self.variables.keys().map(String::as_str)
    }

    /// Values marked secret by Tines, for output redaction by the executor.
    pub fn secret_values(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.run_key.as_str()).chain(
            self.variables
                .values()
                .filter(|entry| entry.secret)
                .map(|entry| entry.value.as_str()),
        )
    }

    /// Add the assignment environment to a command.
    ///
    /// The machine's existing `PATH` remains in effect, and the assignment
    /// cannot replace the Tines-owned API key or API URL.
    pub fn apply_to(&self, command: &mut Command) {
        for entry in self.variables.values() {
            if entry.name != "PATH"
                && entry.name != "TINES_API_KEY"
                && entry.name != "TINES_API_URL"
            {
                command.env(&entry.name, &entry.value);
            }
        }
        command
            .env("TINES_API_KEY", &self.run_key)
            .env("TINES_API_URL", &self.api_url);
    }
}

impl fmt::Debug for LaunchEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchEnvironment")
            .field("variable_names", &self.variables.keys().collect::<Vec<_>>())
            .field("secret_values", &"[REDACTED]")
            .field("api_key", &"[REDACTED]")
            .field("api_url", &self.api_url)
            .finish()
    }
}

/// An error while creating an assignment workspace.
#[derive(Debug)]
pub enum WorkspaceError {
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    InvalidBundle(&'static str),
    InvalidSkillName,
    InvalidSkillPath,
    DuplicateSkillName,
    DuplicateSkillFile,
    InvalidRepositoryDirectory,
    RepositoryOverlapsSkills,
    UnsafeWorkspaceDirectory,
    InvalidEnvironment,
    DuplicateEnvironmentName,
    WorkspaceNameCollision,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "could not {operation}: {source}"),
            Self::InvalidBundle(field) => write!(f, "assignment bundle has invalid {field}"),
            Self::InvalidSkillName => f.write_str("assignment contains an invalid skill name"),
            Self::InvalidSkillPath => f.write_str("assignment contains an unsafe skill file path"),
            Self::DuplicateSkillName => f.write_str("assignment contains duplicate skill names"),
            Self::DuplicateSkillFile => {
                f.write_str("assignment contains duplicate paths in one skill")
            }
            Self::InvalidRepositoryDirectory => {
                f.write_str("assignment contains an unsafe repository directory")
            }
            Self::RepositoryOverlapsSkills => {
                f.write_str("repository directory overlaps the generated skills directory")
            }
            Self::UnsafeWorkspaceDirectory => {
                f.write_str("generated skills path is not a safe workspace directory")
            }
            Self::InvalidEnvironment => {
                f.write_str("assignment contains an invalid environment variable")
            }
            Self::DuplicateEnvironmentName => {
                f.write_str("assignment contains duplicate environment variable names")
            }
            Self::WorkspaceNameCollision => {
                f.write_str("could not allocate a unique assignment workspace")
            }
        }
    }
}

impl Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct BundleFiles {
    skills: Vec<Skill>,
    repos: Vec<Value>,
}

#[derive(Deserialize)]
struct Skill {
    name: String,
    files: Vec<SkillFile>,
}

#[derive(Deserialize)]
struct SkillFile {
    path: String,
    content: String,
}

fn create_unique_workspace(parent: &Path) -> Result<PathBuf, WorkspaceError> {
    for _ in 0..8 {
        let path = parent.join(format!("run-{}", uuid::Uuid::new_v4()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(WorkspaceError::Io {
                    operation: "create assignment workspace",
                    source,
                });
            }
        }
    }
    Err(WorkspaceError::WorkspaceNameCollision)
}

fn materialize_contents(root: &Path, assignment: &RunnerAssignment) -> Result<(), WorkspaceError> {
    let bundle: BundleFiles = serde_json::from_value(assignment.bundle.clone())
        .map_err(|_| WorkspaceError::InvalidBundle("skills or repositories"))?;
    validate_repositories(&bundle.repos)?;
    validate_skills(&bundle.skills)?;

    fs::write(root.join("prompt.md"), format!("{}\n", assignment.prompt)).map_err(|source| {
        WorkspaceError::Io {
            operation: "write assignment prompt",
            source,
        }
    })?;
    let repos = serde_json::to_vec_pretty(&bundle.repos)
        .map_err(|_| WorkspaceError::InvalidBundle("repositories"))?;
    fs::write(root.join("repos.json"), [repos.as_slice(), b"\n"].concat()).map_err(|source| {
        WorkspaceError::Io {
            operation: "write repository metadata",
            source,
        }
    })?;
    materialize_skills(root, &bundle.skills)
}

fn validate_skills(skills: &[Skill]) -> Result<(), WorkspaceError> {
    let mut names = std::collections::BTreeSet::new();
    for skill in skills {
        if skill.name.is_empty()
            || !skill
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(WorkspaceError::InvalidSkillName);
        }
        if !names.insert(skill.name.as_str()) {
            return Err(WorkspaceError::DuplicateSkillName);
        }
        let mut paths = std::collections::BTreeSet::new();
        for file in &skill.files {
            validate_relative_path(&file.path).map_err(|()| WorkspaceError::InvalidSkillPath)?;
            if !paths.insert(file.path.as_str()) {
                return Err(WorkspaceError::DuplicateSkillFile);
            }
        }
    }
    Ok(())
}

fn validate_repositories(repositories: &[Value]) -> Result<(), WorkspaceError> {
    let skill_segments = path_segments(SKILLS_PATH).expect("constant skill path is safe");
    for repository in repositories {
        let dir = repository
            .get("dir")
            .and_then(Value::as_str)
            .ok_or(WorkspaceError::InvalidRepositoryDirectory)?;
        let segments =
            validate_relative_path(dir).map_err(|()| WorkspaceError::InvalidRepositoryDirectory)?;
        if is_prefix(&segments, &skill_segments) || is_prefix(&skill_segments, &segments) {
            return Err(WorkspaceError::RepositoryOverlapsSkills);
        }
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<Vec<&str>, ()> {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') || value.contains('=') {
        return Err(());
    }
    let segments: Vec<_> = value.split('/').collect();
    if segments.iter().any(|segment| {
        segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.contains(':')
            || segment.contains('\0')
    }) {
        return Err(());
    }
    Ok(segments)
}

fn path_segments(value: &str) -> Result<Vec<&str>, ()> {
    validate_relative_path(value)
}

fn is_prefix(left: &[&str], right: &[&str]) -> bool {
    left.len() <= right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn materialize_skills(root: &Path, skills: &[Skill]) -> Result<(), WorkspaceError> {
    // Validate the complete set before touching the existing generated tree.
    validate_skills(skills)?;
    let agents = root.join(".agents");
    let destination = root.join(SKILLS_PATH);
    reject_directory_symlink(&agents)?;
    reject_directory_symlink(&destination)?;
    fs::create_dir_all(&agents).map_err(|source| WorkspaceError::Io {
        operation: "create agent metadata directory",
        source,
    })?;

    let suffix = uuid::Uuid::new_v4();
    let staged = agents.join(format!(".skills-stage-{suffix}"));
    let backup = agents.join(format!(".skills-backup-{suffix}"));
    fs::create_dir(&staged).map_err(|source| WorkspaceError::Io {
        operation: "create staged skill directory",
        source,
    })?;

    let build_result = build_staged_skills(&staged, skills);
    if let Err(error) = build_result {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }

    let had_destination = destination.exists();
    if had_destination {
        fs::rename(&destination, &backup).map_err(|source| WorkspaceError::Io {
            operation: "stage the previous skill directory",
            source,
        })?;
    }
    if let Err(source) = fs::rename(&staged, &destination) {
        if had_destination && !destination.exists() {
            let _ = fs::rename(&backup, &destination);
        }
        let _ = fs::remove_dir_all(&staged);
        return Err(WorkspaceError::Io {
            operation: "install generated skills",
            source,
        });
    }
    if had_destination {
        fs::remove_dir_all(&backup).map_err(|source| WorkspaceError::Io {
            operation: "remove previous generated skills",
            source,
        })?;
    }
    Ok(())
}

fn build_staged_skills(staged: &Path, skills: &[Skill]) -> Result<(), WorkspaceError> {
    for skill in skills {
        let skill_dir = staged.join(&skill.name);
        fs::create_dir(&skill_dir).map_err(|source| WorkspaceError::Io {
            operation: "create skill directory",
            source,
        })?;
        for file in &skill.files {
            let segments = validate_relative_path(&file.path)
                .map_err(|()| WorkspaceError::InvalidSkillPath)?;
            let target = segments
                .iter()
                .fold(skill_dir.clone(), |path, segment| path.join(segment));
            ensure_within(&skill_dir, &target)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|source| WorkspaceError::Io {
                    operation: "create skill file directory",
                    source,
                })?;
            }
            fs::write(&target, &file.content).map_err(|source| WorkspaceError::Io {
                operation: "write skill file",
                source,
            })?;
        }
    }
    Ok(())
}

fn ensure_within(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let mut depth = 0isize;
    for component in path.strip_prefix(root).unwrap_or(path).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => depth -= 1,
            Component::RootDir | Component::Prefix(_) => depth = isize::MIN,
        }
        if depth < 0 {
            return Err(WorkspaceError::InvalidSkillPath);
        }
    }
    Ok(())
}

fn reject_directory_symlink(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(WorkspaceError::UnsafeWorkspaceDirectory)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorkspaceError::Io {
            operation: "inspect workspace directory",
            source,
        }),
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.bytes();
    matches!(characters.next(), Some(b'A'..=b'Z' | b'_'))
        && characters.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::{MaterializedWorkspace, Skill, SkillFile, WorkspaceError, materialize_skills};
    use crate::protocol::RunnerAssignment;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use url::Url;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("tines-runner-workspace-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assignment(bundle: serde_json::Value, env: serde_json::Value) -> RunnerAssignment {
        serde_json::from_value(json!({
            "run": { "id": "arun_test", "issue_id": "iss_test" },
            "prompt": "Prompt body",
            "bundle": bundle,
            "run_key": "secret-run-key",
            "env": env,
            "timeout_minutes": 30
        }))
        .expect("deserialize assignment")
    }

    fn bundle(skill_name: &str, file_path: &str) -> serde_json::Value {
        json!({
            "skills": [{
                "name": skill_name,
                "files": [{ "path": file_path, "content": "skill content" }]
            }],
            "repos": [{
                "name": "fixture",
                "dir": "fixture-repo",
                "url": "https://example.test/fixture.git",
                "branch": "main"
            }]
        })
    }

    #[test]
    fn creates_unique_workspace_tree_and_keeps_environment_out_of_files() {
        let directory = TestDirectory::new();
        let assignment = assignment(
            bundle("fixture-skill", "SKILL.md"),
            json!([
                { "name": "FIXTURE_VALUE", "value": "secret-env-value", "secret": true },
                { "name": "TINES_API_KEY", "value": "attacker-key", "secret": false },
                { "name": "TINES_API_URL", "value": "https://attacker.test", "secret": false },
                { "name": "PATH", "value": "attacker-path", "secret": false }
            ]),
        );
        let api_url = Url::parse("https://tines.example.test/api/").expect("valid URL");

        let first = MaterializedWorkspace::create(&directory.0, &assignment, &api_url)
            .expect("materialize first workspace");
        let second = MaterializedWorkspace::create(&directory.0, &assignment, &api_url)
            .expect("materialize second workspace");

        assert_ne!(first.path(), second.path());
        assert!(first.path().starts_with(&directory.0));
        assert!(!first.path().to_string_lossy().contains("secret-run-key"));
        assert_eq!(
            fs::read_to_string(first.path().join("prompt.md")).expect("read prompt"),
            "Prompt body\n"
        );
        assert_eq!(
            fs::read_to_string(first.path().join(".agents/skills/fixture-skill/SKILL.md"))
                .expect("read skill"),
            "skill content"
        );
        let repos = fs::read_to_string(first.path().join("repos.json")).expect("read repos");
        assert!(repos.contains("fixture-repo"));
        assert!(repos.ends_with('\n'));
        for content in [
            fs::read_to_string(first.path().join("prompt.md")).unwrap(),
            repos,
            fs::read_to_string(first.path().join(".agents/skills/fixture-skill/SKILL.md")).unwrap(),
        ] {
            assert!(!content.contains("secret-run-key"));
            assert!(!content.contains("secret-env-value"));
        }

        let mut command = Command::new("fixture-harness");
        first.environment().apply_to(&mut command);
        let environment: BTreeMap<_, _> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            environment.get("FIXTURE_VALUE").unwrap().as_deref(),
            Some("secret-env-value")
        );
        assert_eq!(
            environment.get("TINES_API_KEY").unwrap().as_deref(),
            Some("secret-run-key")
        );
        assert_eq!(
            environment.get("TINES_API_URL").unwrap().as_deref(),
            Some("https://tines.example.test/api")
        );
        assert!(!environment.contains_key("PATH"));
        assert_eq!(
            first.environment().secret_values().collect::<Vec<_>>(),
            ["secret-run-key", "secret-env-value"]
        );
        let debug = format!("{first:?}");
        assert!(!debug.contains("secret-run-key"));
        assert!(!debug.contains("secret-env-value"));
    }

    #[test]
    fn rejects_invalid_skill_names_and_paths_without_writing_outside_workspace() {
        let cases = [
            ("../escape", "SKILL.md"),
            ("valid-skill", "../escape.txt"),
            ("valid-skill", "/absolute.txt"),
            ("valid-skill", "nested//empty.txt"),
            ("valid-skill", "nested\\escape.txt"),
            ("valid-skill", "nested/name=value.txt"),
        ];
        let api_url = Url::parse("https://tines.example.test").expect("valid URL");

        for (name, path) in cases {
            let directory = TestDirectory::new();
            let outside = directory.0.join("escape.txt");
            fs::write(&outside, "sentinel").expect("write outside sentinel");
            let assignment = assignment(bundle(name, path), json!([]));
            let error = MaterializedWorkspace::create(&directory.0, &assignment, &api_url)
                .expect_err("unsafe skill data must be rejected");
            assert!(matches!(
                error,
                WorkspaceError::InvalidSkillName | WorkspaceError::InvalidSkillPath
            ));
            assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");
            assert_eq!(
                fs::read_dir(&directory.0).unwrap().count(),
                1,
                "failed workspace creation should clean up its run directory"
            );
        }
    }

    #[test]
    fn replaces_only_generated_skills_and_preserves_siblings() {
        let directory = TestDirectory::new();
        let agents = directory.0.join(".agents");
        fs::create_dir_all(&agents).expect("create agents directory");
        fs::write(agents.join("settings.json"), "settings").expect("write sibling");
        let legacy = directory.0.join("skills/legacy/SKILL.md");
        fs::create_dir_all(legacy.parent().unwrap()).expect("create legacy skills");
        fs::write(&legacy, "legacy").expect("write legacy skill");

        materialize_skills(
            &directory.0,
            &[Skill {
                name: "old".to_owned(),
                files: vec![SkillFile {
                    path: "SKILL.md".to_owned(),
                    content: "old".to_owned(),
                }],
            }],
        )
        .expect("write old skills");
        materialize_skills(
            &directory.0,
            &[Skill {
                name: "new".to_owned(),
                files: vec![SkillFile {
                    path: "SKILL.md".to_owned(),
                    content: "new".to_owned(),
                }],
            }],
        )
        .expect("replace skills");

        assert!(!agents.join("skills/old").exists());
        assert_eq!(
            fs::read_to_string(agents.join("skills/new/SKILL.md")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(agents.join("settings.json")).unwrap(),
            "settings"
        );
        assert_eq!(fs::read_to_string(legacy).unwrap(), "legacy");
    }

    #[test]
    fn rejects_repository_directories_that_overlap_generated_skills() {
        let directory = TestDirectory::new();
        let assignment = assignment(
            json!({
                "skills": [],
                "repos": [{ "dir": ".agents/skills/repo" }]
            }),
            json!([]),
        );
        let api_url = Url::parse("https://tines.example.test").unwrap();

        assert!(matches!(
            MaterializedWorkspace::create(&directory.0, &assignment, &api_url),
            Err(WorkspaceError::RepositoryOverlapsSkills)
        ));
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_agents_directory_without_touching_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside = directory.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "safe").unwrap();
        symlink(&outside, workspace.join(".agents")).unwrap();
        let skills = [];

        assert!(matches!(
            materialize_skills(&workspace, &skills),
            Err(WorkspaceError::UnsafeWorkspaceDirectory)
        ));
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "safe"
        );
    }
}
