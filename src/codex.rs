//! Codex invocation construction and safe launch diagnostics.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::assignment::PreparedAssignment;
use crate::effort::{EffortCapabilities, assignment_effort_rejection};
use crate::workspace::LaunchEnvironment;

/// The native-equivalent Codex executable and argv.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexInvocation {
    program: String,
    args: Vec<String>,
}

impl CodexInvocation {
    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Build the native runner's structured-output Codex argv.
///
/// `wrapper` is an argv prefix. Its first word becomes the executable and the
/// remaining words precede `codex` in the argument vector. No shell parses it.
pub fn build_invocation(
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    wrapper: &[String],
) -> Result<CodexInvocation, serde_json::Error> {
    let mut codex_args = vec![
        "exec".to_owned(),
        "--json".to_owned(),
        "--skip-git-repo-check".to_owned(),
    ];
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        codex_args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = effort.filter(|value| !value.is_empty()) {
        codex_args.extend([
            "-c".to_owned(),
            format!("model_reasoning_effort={}", serde_json::to_string(effort)?),
        ]);
    }
    codex_args.push(prompt.to_owned());

    if let Some((program, wrapper_args)) = wrapper.split_first() {
        let mut args = wrapper_args.to_vec();
        args.push("codex".to_owned());
        args.extend(codex_args);
        Ok(CodexInvocation {
            program: program.clone(),
            args,
        })
    } else {
        Ok(CodexInvocation {
            program: "codex".to_owned(),
            args: codex_args,
        })
    }
}

/// A ready-to-spawn Codex command and its assignment launch context.
pub struct CodexLaunch {
    invocation: CodexInvocation,
    working_directory: PathBuf,
    environment: LaunchEnvironment,
    model: Option<String>,
    effort: Option<String>,
    timeout_minutes: Option<u64>,
}

impl CodexLaunch {
    /// Prepare a direct Codex or wrapper command for a resolved assignment.
    pub fn for_assignment(
        assignment: &PreparedAssignment,
        capabilities: &EffortCapabilities,
    ) -> Result<Self, CodexLaunchError> {
        let invocation = build_assignment_invocation(
            assignment.assignment(),
            capabilities,
            &assignment.resolution().config.wrapper,
        )?;

        let mut launch = Self::new(
            invocation,
            assignment.workspace().path(),
            assignment.workspace().environment().clone(),
        );
        launch.model = assignment.assignment().run.model.clone();
        launch.effort = assignment
            .assignment()
            .effort
            .as_ref()
            .map(|effort| effort.value.clone());
        launch.timeout_minutes = Some(assignment.assignment().timeout_minutes);
        Ok(launch)
    }

    /// Combine an invocation with a working directory and assignment environment.
    pub fn new(
        invocation: CodexInvocation,
        working_directory: impl AsRef<Path>,
        environment: LaunchEnvironment,
    ) -> Self {
        Self {
            invocation,
            working_directory: working_directory.as_ref().to_path_buf(),
            environment,
            model: None,
            effort: None,
            timeout_minutes: None,
        }
    }

    pub fn invocation(&self) -> &CodexInvocation {
        &self.invocation
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// Create the child command with argv, cwd, and environment set.
    ///
    /// The caller can spawn this command directly. It must not route it through
    /// a shell, including when a wrapper is configured.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.invocation.program);
        command
            .args(&self.invocation.args)
            .current_dir(&self.working_directory);
        self.environment.apply_to(&mut command);
        command
    }

    /// Format launch information without exposing environment values.
    pub fn format_diagnostics(&self) -> String {
        let argv = std::iter::once(&self.invocation.program)
            .chain(self.invocation.args.iter())
            .collect::<Vec<_>>();
        let mut environment_names = self
            .environment
            .variable_names()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for name in ["TINES_API_KEY", "TINES_API_URL"] {
            if !environment_names.iter().any(|present| present == name) {
                environment_names.push(name.to_owned());
            }
        }
        let model = self.model.as_deref().unwrap_or("(fixed)");
        let effort = self.effort.as_deref().unwrap_or("(provider-default)");
        let timeout = self
            .timeout_minutes
            .map(|minutes| format!("{minutes}m"))
            .unwrap_or_else(|| "(unknown)".to_owned());
        let mut diagnostic = format!(
            "$ {argv:?}\n# tines runner: version={} harness=codex model={model} effort={effort} timeout={timeout} workspace={:?} environment_names={environment_names:?}\n",
            crate::VERSION,
            self.working_directory
        );

        // A secret accidentally repeated in a prompt or wrapper word must not
        // reappear through the argv portion of diagnostics.
        let mut secret_forms = self
            .environment
            .secret_values()
            .filter(|value| !value.is_empty())
            .flat_map(|secret| {
                let json_escaped =
                    serde_json::to_string(secret).expect("a Rust string always serializes to JSON");
                [
                    secret.to_owned(),
                    json_escaped[1..json_escaped.len() - 1].to_owned(),
                ]
            })
            .collect::<Vec<_>>();
        secret_forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
        secret_forms.dedup();
        for secret in secret_forms {
            diagnostic = diagnostic.replace(&secret, "[REDACTED]");
        }
        diagnostic
    }
}

fn build_assignment_invocation(
    assignment: &crate::protocol::RunnerAssignment,
    capabilities: &EffortCapabilities,
    wrapper: &[String],
) -> Result<CodexInvocation, CodexLaunchError> {
    if let Some(reason) = assignment_effort_rejection(assignment, capabilities) {
        return Err(CodexLaunchError::UnsupportedEffort(reason));
    }
    let effort = match assignment.effort.as_ref() {
        Some(effort) if effort.version != 1 => {
            return Err(CodexLaunchError::UnsupportedEffortVersion(effort.version));
        }
        Some(effort) => Some(effort.value.as_str()),
        None => None,
    };
    build_invocation(
        &assignment.prompt,
        assignment.run.model.as_deref(),
        effort,
        wrapper,
    )
    .map_err(CodexLaunchError::SerializeEffort)
}

impl fmt::Debug for CodexLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CodexLaunch")
            .field(&self.format_diagnostics())
            .finish()
    }
}

impl fmt::Display for CodexLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_diagnostics())
    }
}

/// An invalid or unrepresentable assignment launch setting.
#[derive(Debug)]
pub enum CodexLaunchError {
    UnsupportedEffortVersion(u8),
    UnsupportedEffort(String),
    SerializeEffort(serde_json::Error),
}

impl fmt::Display for CodexLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEffortVersion(version) => {
                write!(f, "unsupported assignment effort version {version}")
            }
            Self::UnsupportedEffort(reason) => f.write_str(reason),
            Self::SerializeEffort(error) => write!(f, "could not encode Codex effort: {error}"),
        }
    }
}

impl Error for CodexLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeEffort(error) => Some(error),
            Self::UnsupportedEffortVersion(_) | Self::UnsupportedEffort(_) => None,
        }
    }
}

impl From<serde_json::Error> for CodexLaunchError {
    fn from(error: serde_json::Error) -> Self {
        Self::SerializeEffort(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{CodexLaunch, build_assignment_invocation, build_invocation};
    use crate::effort::{EffortCapabilities, EffortModelCapability};
    use crate::workspace::MaterializedWorkspace;
    use serde_json::json;
    use url::Url;

    fn launch_environment() -> (tempfile_support::TestDirectory, MaterializedWorkspace) {
        let directory = tempfile_support::TestDirectory::new();
        let assignment = serde_json::from_value(json!({
            "run": { "id": "arun_launch", "issue_id": "iss_launch" },
            "prompt": "do work",
            "bundle": { "skills": [], "repos": [] },
            "run_key": "sensitive-run-key",
            "env": [{ "name": "FIXTURE_SECRET", "value": "sensitive-env-value", "secret": true }],
            "timeout_minutes": 30
        }))
        .expect("valid assignment");
        let workspace = MaterializedWorkspace::create(
            directory.path(),
            &assignment,
            &Url::parse("https://tines.example.test/api").unwrap(),
        )
        .expect("materialize workspace");
        (directory, workspace)
    }

    #[test]
    fn codex_invocation_matches_native_structured_output_arguments() {
        let invocation = build_invocation("Implement this issue", Some("gpt-5.1-codex"), None, &[])
            .expect("build invocation");

        assert_eq!(invocation.program(), "codex");
        assert_eq!(
            invocation.args(),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--model",
                "gpt-5.1-codex",
                "Implement this issue"
            ]
        );
    }

    #[test]
    fn wrapper_argv_is_a_direct_prefix_to_the_codex_invocation() {
        let wrapper = vec!["/opt/wrappers/codex proxy".to_owned(), "--trace".to_owned()];
        let invocation = build_invocation("prompt", Some("codex-model"), None, &wrapper)
            .expect("build invocation");

        assert_eq!(invocation.program(), "/opt/wrappers/codex proxy");
        assert_eq!(
            invocation.args(),
            [
                "--trace",
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--model",
                "codex-model",
                "prompt"
            ]
        );
        assert_ne!(invocation.program(), "sh");
    }

    #[test]
    fn resolved_effort_uses_the_codex_config_override_syntax() {
        let invocation =
            build_invocation("prompt", Some("model"), Some("high"), &[]).expect("build invocation");

        assert_eq!(
            invocation.args(),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--model",
                "model",
                "-c",
                "model_reasoning_effort=\"high\"",
                "prompt"
            ]
        );
    }

    #[test]
    fn unsupported_effort_cannot_produce_a_codex_launch_invocation() {
        let assignment: crate::protocol::RunnerAssignment = serde_json::from_value(json!({
            "run": { "id": "arun_launch", "issue_id": "iss_launch", "model": "gpt-5.6" },
            "effort": { "version": 1, "value": "ultra", "capability_digest": "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a" },
            "prompt": "work",
            "bundle": {},
            "run_key": "run-key",
            "timeout_minutes": 30
        }))
        .expect("valid effort assignment");
        let capabilities = EffortCapabilities {
            version: 1,
            daemon_version: "0.1.0".to_owned(),
            harness: "codex".to_owned(),
            harness_version: "codex-cli 0.153.4".to_owned(),
            catalog_digest: "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a"
                .to_owned(),
            models: vec![EffortModelCapability {
                model: "gpt-5.6".to_owned(),
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
            accepts_asserted_effort: None,
            discovery_error: None,
        };

        let error = build_assignment_invocation(&assignment, &capabilities, &[])
            .expect_err("unsupported effort must not create an invocation");

        assert!(error.to_string().contains("does not support effort ultra"));
    }

    #[test]
    fn verified_assignment_effort_is_forwarded_without_substitution() {
        let assignment: crate::protocol::RunnerAssignment = serde_json::from_value(json!({
            "run": { "id": "arun_launch", "issue_id": "iss_launch", "model": "gpt-5.6" },
            "effort": { "version": 1, "value": "high", "capability_digest": "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a" },
            "prompt": "work",
            "bundle": {},
            "run_key": "run-key",
            "timeout_minutes": 30
        }))
        .expect("valid effort assignment");
        let capabilities = EffortCapabilities {
            version: 1,
            daemon_version: "0.1.0".to_owned(),
            harness: "codex".to_owned(),
            harness_version: "codex-cli 0.153.4".to_owned(),
            catalog_digest: "64bb2725f058a9a926043594cf046b5dfbade9206ffa8af7668a9aacd328c98a"
                .to_owned(),
            models: vec![EffortModelCapability {
                model: "gpt-5.6".to_owned(),
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
            accepts_asserted_effort: None,
            discovery_error: None,
        };

        let invocation = build_assignment_invocation(&assignment, &capabilities, &[])
            .expect("supported effort should create an invocation");

        assert!(
            invocation
                .args()
                .contains(&"model_reasoning_effort=\"high\"".to_owned())
        );
        assert!(
            !invocation
                .args()
                .contains(&"model_reasoning_effort=\"low\"".to_owned())
        );
    }

    #[test]
    fn command_uses_workspace_and_assignment_environment_without_logging_secrets() {
        let (_directory, workspace) = launch_environment();
        let invocation = build_invocation(
            "sensitive-run-key",
            Some("gpt-5.6-codex"),
            Some("high"),
            &["wrapper".to_owned()],
        )
        .expect("build invocation");
        assert!(
            invocation
                .args()
                .iter()
                .any(|argument| argument.contains("sensitive-run-key"))
        );
        let mut launch = CodexLaunch::new(
            invocation,
            workspace.path(),
            workspace.environment().clone(),
        );
        launch.timeout_minutes = Some(30);
        launch.model = Some("gpt-5.6-codex".to_owned());
        launch.effort = Some("high".to_owned());
        let command = launch.command();

        assert_eq!(command.get_program(), "wrapper");
        assert_eq!(command.get_args().next().unwrap(), "codex");
        assert_eq!(command.get_current_dir(), Some(workspace.path()));
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment["TINES_API_KEY"],
            Some("sensitive-run-key".to_owned())
        );
        assert_eq!(
            environment["TINES_API_URL"],
            Some("https://tines.example.test/api".to_owned())
        );
        assert_eq!(
            environment["FIXTURE_SECRET"],
            Some("sensitive-env-value".to_owned())
        );

        let diagnostics = launch.format_diagnostics();
        assert!(!diagnostics.contains("sensitive-run-key"));
        assert!(!diagnostics.contains("sensitive-env-value"));
        assert!(diagnostics.contains("FIXTURE_SECRET"));
        assert!(diagnostics.contains("harness=codex"));
        assert!(diagnostics.contains("model=gpt-5.6-codex"));
        assert!(diagnostics.contains("effort=high"));
        assert!(diagnostics.contains("timeout=30m"));
        assert!(diagnostics.contains("workspace="));
        assert!(diagnostics.contains("version="));
        assert!(diagnostics.contains("$ [\"wrapper\""));
        assert!(!format!("{launch:?}").contains("sensitive-run-key"));
    }

    mod tempfile_support {
        use std::path::{Path, PathBuf};

        pub struct TestDirectory(PathBuf);

        impl TestDirectory {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "tines-runner-codex-{}-{}",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                std::fs::create_dir_all(&path).expect("create test directory");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TestDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
