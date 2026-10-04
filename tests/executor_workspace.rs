use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use tines_runner_rs::execution_protocol::{
    ExecutionEventKind, ExecutionEventParser, ExecutionRequest, TerminalStatus,
};
use tines_runner_rs::executor::{prepare_workspace, render_preparation_failure};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tines-runner-executor-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_git(directory: Option<&Path>, arguments: &[&str]) {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let output = command.args(arguments).output().expect("start git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn source_repository(directory: &Path) {
    fs::create_dir_all(directory).expect("create source repository directory");
    run_git(None, &["init", "-b", "main", directory.to_str().unwrap()]);
    run_git(
        Some(directory),
        &["config", "user.email", "workspace-test@example.test"],
    );
    run_git(Some(directory), &["config", "user.name", "Workspace Test"]);
    fs::write(directory.join("README.md"), "main branch\n").expect("write main branch file");
    run_git(Some(directory), &["add", "README.md"]);
    run_git(Some(directory), &["commit", "-m", "main branch"]);
    run_git(Some(directory), &["checkout", "-b", "requested-branch"]);
    fs::write(directory.join("branch.txt"), "requested branch\n")
        .expect("write requested branch file");
    run_git(Some(directory), &["add", "branch.txt"]);
    run_git(Some(directory), &["commit", "-m", "requested branch"]);
}

fn request_fixture(parent: &Path, repo: Value) -> ExecutionRequest {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/execution-request-v1.json"))
            .expect("valid execution request fixture");
    fixture["execution"]["workspace"]["parent"] =
        Value::String(parent.to_string_lossy().into_owned());
    fixture["assignment"]["bundle"]["skills"] = json!([
        {
            "name": "fixture-skill",
            "files": [
                { "path": "SKILL.md", "content": "skill from executor request\n" },
                { "path": "guides/setup.md", "content": "nested skill file\n" }
            ]
        }
    ]);
    fixture["assignment"]["bundle"]["repos"] = json!([repo]);
    serde_json::from_value(fixture).expect("decode execution request fixture")
}

#[test]
fn executor_materializes_request_files_skills_repositories_and_environment() {
    let directory = TestDirectory::new();
    let source = directory.0.join("source");
    source_repository(&source);
    let workspace_parent = directory.0.join("executor-filesystem/workspaces");
    let request = request_fixture(
        &workspace_parent,
        json!({
            "name": "fixture-repo",
            "dir": "checkouts/fixture",
            "url": source.to_string_lossy(),
            "branch": "requested-branch"
        }),
    );

    let workspace = prepare_workspace(&request, |_| {}).expect("prepare executor workspace");
    assert!(workspace.path().starts_with(&workspace_parent));
    assert_eq!(
        fs::read_to_string(workspace.path().join("prompt.md")).unwrap(),
        "Implement the assigned issue.\n"
    );
    let repos: Value = serde_json::from_slice(
        &fs::read(workspace.path().join("repos.json")).expect("read repository metadata"),
    )
    .unwrap();
    assert_eq!(repos, request.assignment.bundle["repos"]);
    assert_eq!(
        fs::read_to_string(
            workspace
                .path()
                .join(".agents/skills/fixture-skill/SKILL.md")
        )
        .unwrap(),
        "skill from executor request\n"
    );
    assert_eq!(
        fs::read_to_string(
            workspace
                .path()
                .join(".agents/skills/fixture-skill/guides/setup.md")
        )
        .unwrap(),
        "nested skill file\n"
    );
    let clone = workspace.path().join("checkouts/fixture");
    assert_eq!(
        fs::read_to_string(clone.join("branch.txt")).unwrap(),
        "requested branch\n"
    );
    assert_eq!(
        fs::read_to_string(clone.join("README.md")).unwrap(),
        "main branch\n"
    );

    let mut command = Command::new("codex");
    command.env("PATH", "executor-path");
    workspace.environment().apply_to(&mut command);
    let environment = command.get_envs().collect::<Vec<_>>();
    let environment_value = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| *key == OsStr::new(name))
            .and_then(|(_, value)| *value)
    };
    assert_eq!(
        environment_value("FIXTURE_TOKEN"),
        Some(OsStr::new("fixture-secret"))
    );
    assert_eq!(
        environment_value("TINES_API_KEY"),
        Some(OsStr::new("fixture-run-key"))
    );
    assert_eq!(
        environment_value("TINES_API_URL"),
        Some(OsStr::new("https://tines.example.test"))
    );
    assert_eq!(environment_value("PATH"), Some(OsStr::new("executor-path")));
    assert!(
        workspace
            .environment()
            .variable_names()
            .any(|name| name == "FIXTURE_TOKEN")
    );
    assert!(
        workspace
            .environment()
            .secret_values()
            .any(|value| value == "fixture-secret")
    );
    assert!(
        workspace
            .environment()
            .secret_values()
            .any(|value| value == "fixture-run-key")
    );
}

#[test]
fn unsafe_repository_paths_become_failed_protocol_results_without_daemon_access() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    let request = request_fixture(
        &workspace_parent,
        json!({
            "name": "unsafe-repo",
            "dir": "../outside",
            "url": "file:///does/not/need/to/be/contacted",
            "branch": null
        }),
    );

    let error = prepare_workspace(&request, |_| {}).expect_err("reject parent path");
    let line = render_preparation_failure(&request, &error).expect("render failure result");
    let mut parser = ExecutionEventParser::default();
    let events = parser.push(line.as_bytes()).expect("parse failure event");
    assert!(parser.finish().unwrap().is_none());
    assert_eq!(events.len(), 1);
    let ExecutionEventKind::Result { result } = &events[0].kind else {
        panic!("workspace error must be a terminal result");
    };
    assert_eq!(result.status, TerminalStatus::Failed);
    assert_eq!(result.exit_code, Some(1));
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .contains("unsafe repository directory")
    );
    assert!(!line.contains("fixture-run-key"));
    assert!(!line.contains("fixture-secret"));
    assert!(!directory.0.join("outside").exists());
    assert!(
        fs::read_dir(&workspace_parent)
            .expect("read workspace parent")
            .next()
            .is_none(),
        "failed preparation removes its partial workspace"
    );
}

#[test]
fn repository_clone_and_branch_failures_remove_partial_workspaces() {
    let directory = TestDirectory::new();
    let source = directory.0.join("source");
    source_repository(&source);
    let workspace_parent = directory.0.join("workspaces");
    fs::create_dir_all(&workspace_parent).expect("create workspace parent");

    for repo in [
        json!({
            "name": "missing-repo",
            "dir": "repo",
            "url": directory.0.join("missing-repo").to_string_lossy(),
            "branch": null
        }),
        json!({
            "name": "missing-branch",
            "dir": "repo",
            "url": source.to_string_lossy(),
            "branch": "missing-branch"
        }),
    ] {
        let request = request_fixture(&workspace_parent, repo);
        let error = prepare_workspace(&request, |_| {}).expect_err("clone should fail");
        let line = render_preparation_failure(&request, &error).expect("render failure result");
        let event: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(event["type"], "result");
        assert_eq!(event["status"], "failed");
        assert_eq!(event["exit_code"], 1);
        assert!(
            event["error"]
                .as_str()
                .unwrap()
                .contains("git clone failed")
        );
        assert!(
            fs::read_dir(&workspace_parent)
                .expect("read workspace parent")
                .next()
                .is_none(),
            "failed clone removes its partial workspace"
        );
    }
}

#[test]
fn clone_failure_protocol_results_redact_escaped_secret_directories() {
    let directory = TestDirectory::new();
    let workspace_parent = directory.0.join("workspaces");
    fs::create_dir_all(&workspace_parent).expect("create workspace parent");

    for secret in ["private\"value", "private\nvalue"] {
        let mut request = request_fixture(
            &workspace_parent,
            json!({
                "name": "missing-repo",
                "dir": secret,
                "url": directory.0.join("missing-repository").to_string_lossy(),
                "branch": null
            }),
        );
        request.assignment.env[0].value = secret.to_owned();

        let error = prepare_workspace(&request, |_| {}).expect_err("clone should fail");
        let line = render_preparation_failure(&request, &error).expect("render failure result");
        let event: Value = serde_json::from_str(line.trim_end()).unwrap();
        let error = event["error"].as_str().expect("failure result has error");
        let rust_escaped = format!("{secret:?}");
        let rust_escaped = &rust_escaped[1..rust_escaped.len() - 1];

        assert!(error.contains("git clone failed"));
        assert!(error.contains("***"));
        assert!(!error.contains(secret), "raw secret leaked for {secret:?}");
        assert!(
            !error.contains(rust_escaped),
            "escaped secret leaked for {secret:?}"
        );
        assert!(!line.contains(rust_escaped));
        assert!(
            fs::read_dir(&workspace_parent)
                .expect("read workspace parent")
                .next()
                .is_none(),
            "failed clone removes its partial workspace"
        );
    }
}
