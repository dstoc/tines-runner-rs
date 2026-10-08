//! Antigravity invocation construction and bounded capability discovery.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::effort::{EffortCapabilities, EffortModelCapability, catalog_digest};
use crate::execution_protocol::ExecutionRequest;
use crate::executor::workspace::MaterializedWorkspace;

const DISCOVERY_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DISCOVERY_OUTPUT: usize = 1024 * 1024;
const MAX_VERSION_LENGTH: usize = 100;
const MAX_MODELS: usize = 256;
const MAX_MODEL_SLUG_LENGTH: usize = 200;
const MAX_MODEL_LINE_LENGTH: usize = 4096;
const MINIMUM_VERSION: (u64, u64, u64) = (1, 3, 1);

/// Antigravity version and model catalog observed by the executor.
#[derive(Clone, Debug)]
pub struct AntigravityDiscovery {
    pub version: Option<String>,
    pub effort: Option<EffortCapabilities>,
    pub error: Option<String>,
}

/// A directly spawned Antigravity CLI command and its prompt input.
pub struct AntigravityLaunch {
    program: &'static str,
    args: Vec<String>,
    stdin: Vec<u8>,
    working_directory: std::path::PathBuf,
}

impl AntigravityLaunch {
    /// Build a headless stream-JSON invocation for one assignment.
    pub fn for_execution_request(
        request: &ExecutionRequest,
        workspace: &MaterializedWorkspace,
    ) -> Result<Self, &'static str> {
        if request.execution.harness != "antigravity" {
            return Err("execution request does not match the Antigravity adapter");
        }
        if request.assignment.effort.is_some() {
            return Err(
                "Antigravity explicit effort delivery is not supported by this runner version",
            );
        }

        let mut args = vec![
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--sandbox".to_owned(),
            "--print-timeout".to_owned(),
            request
                .assignment
                .timeout_minutes
                .saturating_add(1)
                .to_string()
                + "m",
        ];
        if let Some(model) = request.assignment.run.model.as_deref() {
            args.extend(["--model".to_owned(), model.to_owned()]);
        }

        let mut stdin = serde_json::to_vec(&serde_json::json!({
            "event": "user",
            "message": { "content": &request.assignment.prompt },
        }))
        .map_err(|_| "could not encode Antigravity stream input")?;
        stdin.push(b'\n');

        Ok(Self {
            program: "agy",
            args,
            stdin,
            working_directory: workspace.path().to_path_buf(),
        })
    }

    pub fn program(&self) -> &str {
        self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn stdin(&self) -> &[u8] {
        &self.stdin
    }

    pub fn working_directory(&self) -> &std::path::Path {
        &self.working_directory
    }

    pub fn command(&self, workspace: &MaterializedWorkspace) -> Command {
        let mut command = Command::new(self.program);
        command
            .args(&self.args)
            .current_dir(&self.working_directory);
        workspace.environment().apply_to(&mut command);
        command
    }

    /// Render the invocation without exposing assignment prompts or secrets.
    pub fn diagnostics(&self, request: &ExecutionRequest) -> String {
        let secrets = request.secret_patterns();
        let argv = std::iter::once(self.program.to_owned())
            .chain(self.args.iter().cloned())
            .map(|value| redact(&value, &secrets))
            .collect::<Vec<_>>();
        let cwd = redact(&self.working_directory.to_string_lossy(), &secrets);
        format!(
            "$ {argv:?}\n# tines runner: version={} harness=antigravity cwd=workspace workspace={cwd:?}\n",
            crate::VERSION,
        )
    }
}

/// Probe `agy --version` and `agy models` in the current executor environment.
pub fn discover(daemon_version: &str) -> AntigravityDiscovery {
    discover_with_program("agy", daemon_version)
}

fn discover_with_program(program: &str, daemon_version: &str) -> AntigravityDiscovery {
    discover_with_program_and_timeout(program, daemon_version, DISCOVERY_COMMAND_TIMEOUT)
}

fn discover_with_program_and_timeout(
    program: &str,
    daemon_version: &str,
    command_timeout: Duration,
) -> AntigravityDiscovery {
    let version_output = match run_capture(program, &["--version"], command_timeout) {
        Ok(output) => output,
        Err(error) => {
            return AntigravityDiscovery {
                version: None,
                effort: None,
                error: Some(error),
            };
        }
    };
    let Ok(version) = String::from_utf8(version_output) else {
        return AntigravityDiscovery {
            version: None,
            effort: None,
            error: Some("agy version output was not UTF-8".to_owned()),
        };
    };
    let version = version.trim();
    if version.is_empty() || version.chars().count() > MAX_VERSION_LENGTH {
        return AntigravityDiscovery {
            version: None,
            effort: None,
            error: Some("agy returned an invalid version".to_owned()),
        };
    }
    let Some(parsed_version) = version_tuple(version) else {
        return AntigravityDiscovery {
            version: None,
            effort: None,
            error: Some("agy version could not be determined".to_owned()),
        };
    };
    if parsed_version < MINIMUM_VERSION {
        return AntigravityDiscovery {
            version: None,
            effort: None,
            error: Some("agy 1.3.1 or newer is required".to_owned()),
        };
    }

    let models_output = match run_capture(program, &["models"], command_timeout) {
        Ok(output) => output,
        Err(error) => {
            return AntigravityDiscovery {
                version: Some(version.to_owned()),
                effort: None,
                error: Some(error),
            };
        }
    };
    let Ok(models_output) = String::from_utf8(models_output) else {
        return AntigravityDiscovery {
            version: Some(version.to_owned()),
            effort: None,
            error: Some("agy model output was not UTF-8".to_owned()),
        };
    };
    let models = match parse_models(&models_output) {
        Ok(models) => models,
        Err(error) => {
            return AntigravityDiscovery {
                version: Some(version.to_owned()),
                effort: None,
                error: Some(error),
            };
        }
    };
    if models.is_empty() {
        let error = if models_output.trim().is_empty() {
            "agy model catalog was empty"
        } else {
            "agy model output contained no recognized model slugs"
        };
        return AntigravityDiscovery {
            version: Some(version.to_owned()),
            effort: None,
            error: Some(error.to_owned()),
        };
    }
    let catalog_digest = catalog_digest(&models);
    AntigravityDiscovery {
        version: Some(version.to_owned()),
        effort: Some(EffortCapabilities {
            version: 1,
            daemon_version: daemon_version.chars().take(100).collect(),
            harness: "antigravity".to_owned(),
            harness_version: version.to_owned(),
            catalog_digest,
            models,
            accepts_asserted_effort: None,
            discovery_error: None,
        }),
        error: None,
    }
}

/// Extract model slugs from the stable first field of each listing line.
pub fn parse_models(output: &str) -> Result<Vec<EffortModelCapability>, String> {
    if output.len() > MAX_DISCOVERY_OUTPUT {
        return Err("agy model output exceeded 1 MiB".to_owned());
    }
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.len() > MAX_MODEL_LINE_LENGTH {
            continue;
        }
        let Some(slug) = line.split_whitespace().next() else {
            continue;
        };
        if !valid_model_slug(slug) || !seen.insert(slug.to_owned()) {
            continue;
        }
        if models.len() == MAX_MODELS {
            return Err("agy model catalog exceeded 256 models".to_owned());
        }
        models.push(EffortModelCapability {
            model: slug.to_owned(),
            efforts: Vec::new(),
        });
    }
    Ok(models)
}

fn valid_model_slug(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > MAX_MODEL_SLUG_LENGTH {
        return false;
    }
    let mut bytes = slug.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn version_tuple(output: &str) -> Option<(u64, u64, u64)> {
    output.split_whitespace().find_map(|token| {
        let token = token
            .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.');
        let token = token.strip_prefix('v').unwrap_or(token);
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some((major, minor, patch))
    })
}

fn run_capture(program: &str, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| format!("could not start {}", command_label(args)))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || read_bounded(stdout, sender));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => break Err("command timed out".to_owned()),
            Err(_) => break Err("could not wait for command".to_owned()),
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} {error}", command_label(args)));
        }
    };
    let output = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| format!("could not read {} output", command_label(args)))??;
    let _ = reader.join();
    if !status.success() {
        return Err(format!("{} exited unsuccessfully", command_label(args)));
    }
    Ok(output)
}

fn command_label(args: &[&str]) -> &'static str {
    if args.first() == Some(&"--version") {
        "agy --version"
    } else {
        "agy models"
    }
}

fn read_bounded(mut reader: impl Read, sender: mpsc::Sender<Result<Vec<u8>, String>>) {
    let mut output = Vec::new();
    let mut oversized = false;
    let mut chunk = [0; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) if output.len().saturating_add(count) <= MAX_DISCOVERY_OUTPUT => {
                if !oversized {
                    output.extend_from_slice(&chunk[..count]);
                }
            }
            Ok(_) => {
                oversized = true;
                output.clear();
            }
            Err(_) => {
                let _ = sender.send(Err("could not read agy discovery output".to_owned()));
                return;
            }
        }
    }
    let result = if oversized {
        Err("agy discovery output exceeded 1 MiB".to_owned())
    } else {
        Ok(output)
    };
    let _ = sender.send(result);
}

fn redact(value: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .fold(value.to_owned(), |safe, secret| safe.replace(secret, "***"))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_MODELS, discover_with_program, discover_with_program_and_timeout, parse_models,
        version_tuple,
    };

    #[test]
    fn parses_model_slugs_in_order_and_deduplicates_without_effort_inference() {
        let models = parse_models(
            "\n gemini-3.8-flash-high  Gemini 3.8 Flash (High)\nclaude-opus-5-5-medium Claude\ngemini-3.8-flash-high duplicate\n",
        )
        .expect("model output is valid");
        assert_eq!(
            models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>(),
            ["gemini-3.8-flash-high", "claude-opus-5-5-medium"]
        );
        assert!(models.iter().all(|model| model.efforts.is_empty()));
    }

    #[test]
    fn ignores_blank_invalid_and_overlong_model_lines() {
        let output = format!(
            "\nnot/a/model Display\n{} Description\nvalid-model Display\n",
            "x".repeat(201)
        );
        let models = parse_models(&output).expect("invalid rows are skipped");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "valid-model");
        let long_line = format!("valid-but-too-long {}", "x".repeat(4096));
        assert!(parse_models(&long_line).unwrap().is_empty());
        assert!(parse_models(&"x".repeat(1024 * 1024 + 1)).is_err());
    }

    #[test]
    fn rejects_catalogs_that_exceed_the_model_count_limit() {
        let output = (0..=MAX_MODELS)
            .map(|index| format!("model-{index} Description\n"))
            .collect::<String>();
        assert!(parse_models(&output).is_err());
    }

    #[test]
    fn parses_supported_version_forms_and_rejects_unknown_text() {
        assert_eq!(version_tuple("agy v1.3.1"), Some((1, 3, 1)));
        assert_eq!(version_tuple("1.4.0"), Some((1, 4, 0)));
        assert_eq!(version_tuple("agy current"), None);
        assert_eq!(version_tuple("1.3.1-beta"), None);
    }

    #[cfg(unix)]
    #[test]
    fn discovers_version_and_models_from_the_executor_program() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("tines-agy-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("create discovery directory");
        let program = directory.join("agy");
        std::fs::write(
            &program,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' 'agy v1.3.1'; else printf '%s\\n' 'not/a/model Unrecognized row' 'gemini-3.8-flash-high Gemini 3.8 Flash (High)' 'claude-opus-5-5-medium Claude Opus (Medium)' 'gemini-3.8-flash-high duplicate'; fi\n",
        )
        .expect("write agy fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make agy fixture executable");

        let discovery = discover_with_program(program.to_str().expect("program path"), "0.1.0");
        assert_eq!(discovery.version.as_deref(), Some("agy v1.3.1"));
        assert_eq!(discovery.error, None);
        let effort = discovery.effort.expect("discovered model catalog");
        assert_eq!(effort.harness, "antigravity");
        assert_eq!(effort.harness_version, "agy v1.3.1");
        assert_eq!(
            effort
                .models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>(),
            ["gemini-3.8-flash-high", "claude-opus-5-5-medium"]
        );
        assert!(effort.models.iter().all(|model| model.efforts.is_empty()));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn version_and_model_commands_have_independent_deadlines() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let directory = std::env::temp_dir().join(format!(
            "tines-agy-independent-timeouts-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create discovery directory");
        let program = directory.join("agy");
        std::fs::write(
            &program,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then sleep 1.2; printf '%s\\n' 'agy v1.3.1'; else sleep 1.2; printf '%s\\n' 'gemini-3.8-flash-high Gemini 3.8 Flash'; fi\n",
        )
        .expect("write slow agy fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make agy fixture executable");

        let started = Instant::now();
        let discovery = discover_with_program_and_timeout(
            program.to_str().expect("program path"),
            "0.1.0",
            Duration::from_secs(2),
        );

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(discovery.error, None);
        assert_eq!(discovery.effort.expect("model catalog").models.len(), 1);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_empty_and_unrecognized_model_catalogs() {
        use std::os::unix::fs::PermissionsExt;

        let cases = [
            ("", "agy model catalog was empty"),
            (" \n\t \n", "agy model catalog was empty"),
            (
                "not/a/model Display\n---\ninvalid!slug Name\n",
                "agy model output contained no recognized model slugs",
            ),
        ];

        for (model_output, expected_error) in cases {
            let directory = std::env::temp_dir()
                .join(format!("tines-agy-empty-catalog-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).expect("create discovery directory");
            let program = directory.join("agy");
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' 'agy v1.3.1'; else printf '%s' '{model_output}'; fi\n"
            );
            std::fs::write(&program, script).expect("write agy fixture");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                .expect("make agy fixture executable");

            let discovery = discover_with_program(program.to_str().expect("program path"), "0.1.0");

            assert_eq!(discovery.version.as_deref(), Some("agy v1.3.1"));
            assert!(discovery.effort.is_none());
            assert_eq!(discovery.error.as_deref(), Some(expected_error));
            let _ = std::fs::remove_dir_all(directory);
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_command_timeout_is_bounded() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let directory = std::env::temp_dir().join(format!(
            "tines-agy-command-timeout-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create discovery directory");
        let program = directory.join("agy");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 5\n").expect("write slow agy fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make agy fixture executable");

        let started = Instant::now();
        let discovery = discover_with_program_and_timeout(
            program.to_str().expect("program path"),
            "0.1.0",
            Duration::from_millis(100),
        );

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(discovery.effort, None);
        assert_eq!(
            discovery.error.as_deref(),
            Some("agy --version command timed out")
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_advertise_unsupported_or_undetermined_versions() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("tines-agy-version-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("create version directory");
        let program = directory.join("agy");
        for (version, expected_error) in [
            ("agy 1.2.9", "agy 1.3.1 or newer is required"),
            ("agy latest", "agy version could not be determined"),
        ] {
            std::fs::write(&program, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"))
                .expect("write agy version fixture");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                .expect("make agy fixture executable");
            let discovery = discover_with_program(program.to_str().expect("program path"), "0.1.0");
            assert!(discovery.version.is_none());
            assert!(discovery.effort.is_none());
            assert_eq!(discovery.error.as_deref(), Some(expected_error));
        }
        let _ = std::fs::remove_dir_all(directory);
    }
}
