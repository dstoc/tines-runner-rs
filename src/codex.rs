//! Codex invocation construction and safe launch diagnostics.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::assignment::PreparedAssignment;
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
}

impl CodexLaunch {
    /// Prepare a direct Codex or wrapper command for a resolved assignment.
    pub fn for_assignment(assignment: &PreparedAssignment) -> Result<Self, CodexLaunchError> {
        let payload = assignment.assignment();
        let effort = match payload.effort.as_ref() {
            Some(effort) if effort.version != 1 => {
                return Err(CodexLaunchError::UnsupportedEffortVersion(effort.version));
            }
            Some(effort) => Some(effort.value.as_str()),
            None => None,
        };
        let invocation = build_invocation(
            &payload.prompt,
            payload.run.model.as_deref(),
            effort,
            &assignment.resolution().config.wrapper,
        )
        .map_err(CodexLaunchError::SerializeEffort)?;

        Ok(Self::new(
            invocation,
            assignment.workspace().path(),
            assignment.workspace().environment().clone(),
        ))
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
        let mut diagnostic = format!(
            "argv={argv:?} cwd={:?} environment_names={environment_names:?}",
            self.working_directory
        );

        // A secret accidentally repeated in a prompt or wrapper word must not
        // reappear through the argv portion of diagnostics.
        for secret in self
            .environment
            .secret_values()
            .filter(|value| !value.is_empty())
        {
            diagnostic = diagnostic.replace(secret, "[REDACTED]");
        }
        diagnostic
    }
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
    SerializeEffort(serde_json::Error),
}

impl fmt::Display for CodexLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEffortVersion(version) => {
                write!(f, "unsupported assignment effort version {version}")
            }
            Self::SerializeEffort(error) => write!(f, "could not encode Codex effort: {error}"),
        }
    }
}

impl Error for CodexLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeEffort(error) => Some(error),
            Self::UnsupportedEffortVersion(_) => None,
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
    use super::{CodexLaunch, build_invocation};
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
    fn command_uses_workspace_and_assignment_environment_without_logging_secrets() {
        let (_directory, workspace) = launch_environment();
        let invocation = build_invocation("prompt", None, None, &["wrapper".to_owned()])
            .expect("build invocation");
        assert!(
            invocation
                .args()
                .iter()
                .all(|argument| !argument.contains("sensitive-run-key"))
        );
        let launch = CodexLaunch::new(
            invocation,
            workspace.path(),
            workspace.environment().clone(),
        );
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
