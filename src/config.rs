//! TOML configuration and assignment-specific execution policy.

use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

/// The harness types supported by the runner.
///
/// Add variants here when the runner gains another harness. The initial
/// configuration format accepts only `codex`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunnerType {
    Codex,
}

/// Whether a completed assignment's workspace should be retained.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RetentionMode {
    #[default]
    Never,
    Failed,
    Always,
}

/// Bounds and mode for retained assignment workspaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRetention {
    pub mode: RetentionMode,
    pub max_age: Duration,
    pub max_count: usize,
}

/// Non-secret runner configuration after parsing and path expansion.
#[derive(Clone, Debug)]
pub struct Config {
    pub server_url: Url,
    pub runner_name: String,
    pub runner_type: RunnerType,
    pub workspace_parent: PathBuf,
    /// An argv prefix. Entries are passed directly to process creation.
    pub wrapper: Vec<String>,
    pub max_concurrent: usize,
    pub poll_interval: Duration,
    pub credentials_file: PathBuf,
    pub workspace_retention: WorkspaceRetention,
    overrides: Vec<ConfigOverride>,
}

/// The immutable execution settings resolved for one assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRunConfig {
    pub runner_type: RunnerType,
    pub workspace_parent: PathBuf,
    /// An argv prefix. It is not a shell command.
    pub wrapper: Vec<String>,
}

/// Names used to select assignment-specific overrides.
#[derive(Clone, Copy, Debug)]
pub struct MatchContext<'a> {
    pub project: &'a str,
    pub workflow: &'a str,
    pub state: &'a str,
}

/// An error while loading or validating runner configuration.
#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "could not read config file {}: {source}", path.display())
            }
            Self::Parse(source) => write!(f, "invalid config TOML: {source}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

impl Config {
    /// Return the platform/XDG default location for `config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        Ok(default_paths()?.config_dir.join("config.toml"))
    }

    /// Load configuration from the platform/XDG default location.
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(Self::default_path()?)
    }

    /// Load configuration from a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&contents)
    }

    /// Parse configuration TOML and expand path settings using the current
    /// home and platform/XDG directories.
    pub fn from_toml_str(contents: &str) -> Result<Self, ConfigError> {
        Self::from_toml_str_with_defaults(contents, default_paths()?)
    }

    /// Resolve all matching overrides in declaration order.
    ///
    /// The returned value owns its fields and does not borrow from or retain
    /// the parsed configuration.
    pub fn resolve(&self, context: MatchContext<'_>) -> ResolvedRunConfig {
        let mut resolved = ResolvedRunConfig {
            runner_type: self.runner_type,
            workspace_parent: self.workspace_parent.clone(),
            wrapper: self.wrapper.clone(),
        };

        for rule in &self.overrides {
            if !rule.matches(context) {
                continue;
            }
            if let Some(runner_type) = rule.runner_type {
                resolved.runner_type = runner_type;
            }
            if let Some(workspace_parent) = &rule.workspace_parent {
                resolved.workspace_parent.clone_from(workspace_parent);
            }
            if let Some(wrapper) = &rule.wrapper {
                resolved.wrapper.clone_from(wrapper);
            }
        }

        resolved
    }

    fn from_toml_str_with_defaults(
        contents: &str,
        defaults: DefaultPaths,
    ) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(contents).map_err(ConfigError::Parse)?;

        let server_url = raw
            .server
            .url
            .ok_or_else(|| ConfigError::Invalid("missing [server].url".to_owned()))?
            .parse::<Url>()
            .map_err(|error| ConfigError::Invalid(format!("invalid server URL: {error}")))?;
        if !matches!(server_url.scheme(), "http" | "https") || server_url.host().is_none() {
            return Err(ConfigError::Invalid(
                "server URL must be an absolute HTTP or HTTPS URL".to_owned(),
            ));
        }

        let runner_name = raw
            .runner
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ConfigError::Invalid("missing [runner].name".to_owned()))?;
        let workspace_parent = expand_path(
            raw.runner
                .workspace_parent
                .as_deref()
                .unwrap_or(&defaults.workspace_parent),
            &defaults.home,
        )?;
        let credentials_file = expand_path(
            raw.storage
                .credentials_file
                .as_deref()
                .unwrap_or(&defaults.credentials_file),
            &defaults.home,
        )?;

        let max_concurrent = raw.runner.max_concurrent.unwrap_or(1);
        if max_concurrent == 0 {
            return Err(ConfigError::Invalid(
                "[runner].max_concurrent must be greater than zero".to_owned(),
            ));
        }
        let poll_interval_seconds = raw.runner.poll_interval_seconds.unwrap_or(15);
        if poll_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "[runner].poll_interval_seconds must be greater than zero".to_owned(),
            ));
        }

        let keep_workspaces_for_hours = raw.storage.keep_workspaces_for_hours.unwrap_or(72);
        let keep_workspaces_max = raw.storage.keep_workspaces_max.unwrap_or(20);
        let keep_workspaces_max_age_seconds = keep_workspaces_for_hours
            .checked_mul(60 * 60)
            .ok_or_else(|| {
                ConfigError::Invalid("[storage].keep_workspaces_for_hours is too large".to_owned())
            })?;
        let overrides = raw
            .overrides
            .into_iter()
            .map(|rule| {
                if rule.workspace_parent.is_none()
                    && rule.runner_type.is_none()
                    && rule.wrapper.is_none()
                {
                    return Err(ConfigError::Invalid(
                        "each [[override]] must set at least one override value".to_owned(),
                    ));
                }
                Ok(ConfigOverride {
                    project: rule.project,
                    workflow: rule.workflow,
                    state: rule.state,
                    workspace_parent: rule
                        .workspace_parent
                        .as_deref()
                        .map(|path| expand_path(path, &defaults.home))
                        .transpose()?,
                    runner_type: rule.runner_type,
                    wrapper: rule.wrapper,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;

        Ok(Self {
            server_url,
            runner_name,
            runner_type: raw.runner.runner_type.unwrap_or(RunnerType::Codex),
            workspace_parent,
            wrapper: raw.runner.wrapper.unwrap_or_default(),
            max_concurrent,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            credentials_file,
            workspace_retention: WorkspaceRetention {
                mode: raw.storage.keep_workspaces.unwrap_or_default(),
                max_age: Duration::from_secs(keep_workspaces_max_age_seconds),
                max_count: keep_workspaces_max,
            },
            overrides,
        })
    }
}

#[derive(Clone, Debug)]
struct ConfigOverride {
    project: Option<String>,
    workflow: Option<String>,
    state: Option<String>,
    workspace_parent: Option<PathBuf>,
    runner_type: Option<RunnerType>,
    wrapper: Option<Vec<String>>,
}

impl ConfigOverride {
    fn matches(&self, context: MatchContext<'_>) -> bool {
        self.project
            .as_deref()
            .is_none_or(|name| name.eq_ignore_ascii_case(context.project))
            && self
                .workflow
                .as_deref()
                .is_none_or(|name| name.eq_ignore_ascii_case(context.workflow))
            && self
                .state
                .as_deref()
                .is_none_or(|name| name.eq_ignore_ascii_case(context.state))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    runner: RawRunner,
    #[serde(default)]
    storage: RawStorage,
    #[serde(default, rename = "override")]
    overrides: Vec<RawOverride>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRunner {
    name: Option<String>,
    runner_type: Option<RunnerType>,
    workspace_parent: Option<PathBuf>,
    wrapper: Option<Vec<String>>,
    max_concurrent: Option<usize>,
    poll_interval_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    credentials_file: Option<PathBuf>,
    keep_workspaces: Option<RetentionMode>,
    keep_workspaces_for_hours: Option<u64>,
    keep_workspaces_max: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOverride {
    project: Option<String>,
    workflow: Option<String>,
    state: Option<String>,
    workspace_parent: Option<PathBuf>,
    runner_type: Option<RunnerType>,
    wrapper: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
struct DefaultPaths {
    home: PathBuf,
    config_dir: PathBuf,
    workspace_parent: PathBuf,
    credentials_file: PathBuf,
}

fn default_paths() -> Result<DefaultPaths, ConfigError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            ConfigError::Invalid(
                "could not determine the home directory from HOME or USERPROFILE".to_owned(),
            )
        })?;

    Ok(default_paths_for(
        home,
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        env::var_os("APPDATA").map(PathBuf::from),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
    ))
}

fn default_paths_for(
    home: PathBuf,
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    _appdata: Option<PathBuf>,
    _local_appdata: Option<PathBuf>,
) -> DefaultPaths {
    #[cfg(windows)]
    let config_dir = absolute_path(xdg_config_home)
        .or_else(|| absolute_path(_appdata))
        .unwrap_or_else(|| home.join("AppData").join("Roaming"))
        .join("tines-runner-rs");
    #[cfg(target_os = "macos")]
    let config_dir = absolute_path(xdg_config_home)
        .unwrap_or_else(|| home.join("Library").join("Application Support"))
        .join("tines-runner-rs");
    #[cfg(all(unix, not(target_os = "macos")))]
    let config_dir = absolute_path(xdg_config_home)
        .unwrap_or_else(|| home.join(".config"))
        .join("tines-runner-rs");

    #[cfg(windows)]
    let data_dir = absolute_path(xdg_data_home)
        .or_else(|| absolute_path(_local_appdata))
        .unwrap_or_else(|| home.join("AppData").join("Local"));
    #[cfg(target_os = "macos")]
    let data_dir = absolute_path(xdg_data_home)
        .unwrap_or_else(|| home.join("Library").join("Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let data_dir =
        absolute_path(xdg_data_home).unwrap_or_else(|| home.join(".local").join("share"));

    DefaultPaths {
        home,
        credentials_file: config_dir.join("credentials.toml"),
        config_dir,
        workspace_parent: data_dir.join("tines-runner-rs").join("workspaces"),
    }
}

fn absolute_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| path.is_absolute())
}

fn expand_path(path: &Path, home: &Path) -> Result<PathBuf, ConfigError> {
    if path == Path::new("~") {
        return Ok(home.to_path_buf());
    }
    if let Ok(suffix) = path.strip_prefix("~") {
        return Ok(home.join(suffix));
    }

    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> DefaultPaths {
        let home = PathBuf::from("/home/tester");
        let config_dir = home.join(".config/tines-runner-rs");
        DefaultPaths {
            home: home.clone(),
            config_dir: config_dir.clone(),
            workspace_parent: home.join(".local/share/tines-runner-rs/workspaces"),
            credentials_file: config_dir.join("credentials.toml"),
        }
    }

    fn parse_with_overrides(overrides: &str) -> Config {
        let contents = format!(
            r#"
                [server]
                url = "https://tines.example.test"

                [runner]
                name = "test-runner"
                workspace_parent = "/default/workspaces"
                wrapper = ["base-wrapper", "--"]

                {overrides}
            "#
        );
        Config::from_toml_str_with_defaults(&contents, defaults()).unwrap()
    }

    fn context<'a>(project: &'a str, workflow: &'a str, state: &'a str) -> MatchContext<'a> {
        MatchContext {
            project,
            workflow,
            state,
        }
    }

    #[test]
    fn project_only_override_matches_exact_name_case_insensitively() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
workspace_parent = "/project"
"#,
        );

        assert_eq!(
            config
                .resolve(context("tInEs", "Build", "Implement"))
                .workspace_parent,
            PathBuf::from("/project")
        );
        assert_eq!(
            config
                .resolve(context("Tines Tools", "Build", "Implement"))
                .workspace_parent,
            PathBuf::from("/default/workspaces")
        );
    }

    #[test]
    fn workflow_only_override_matches_by_workflow() {
        let config = parse_with_overrides(
            r#"[[override]]
workflow = "Implementation"
wrapper = ["impl-wrapper", "--"]
"#,
        );

        assert_eq!(
            config
                .resolve(context("Other", "implementation", "Review"))
                .wrapper,
            ["impl-wrapper", "--"]
        );
        assert_eq!(
            config
                .resolve(context("Other", "Implement", "Review"))
                .wrapper,
            ["base-wrapper", "--"]
        );
    }

    #[test]
    fn state_only_override_matches_by_state() {
        let config = parse_with_overrides(
            r#"[[override]]
state = "Review"
runner_type = "codex"
"#,
        );

        assert_eq!(
            config
                .resolve(context("Other", "Other", "review"))
                .runner_type,
            RunnerType::Codex
        );
    }

    #[test]
    fn combined_override_requires_every_selector_to_match() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
workflow = "Implementation"
state = "Review"
wrapper = ["review-wrapper"]
"#,
        );

        assert_eq!(
            config
                .resolve(context("tines", "implementation", "review"))
                .wrapper,
            ["review-wrapper"]
        );
        assert_eq!(
            config
                .resolve(context("Tines", "Implementation", "Implement"))
                .wrapper,
            ["base-wrapper", "--"]
        );
    }

    #[test]
    fn overlapping_overrides_apply_in_declaration_order_by_field() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
workspace_parent = "/project"
wrapper = ["project-wrapper"]

[[override]]
workflow = "Implementation"
wrapper = ["workflow-wrapper"]

[[override]]
state = "Review"
workspace_parent = "/review"
"#,
        );

        let resolved = config.resolve(context("Tines", "Implementation", "Review"));
        assert_eq!(resolved.workspace_parent, PathBuf::from("/review"));
        assert_eq!(resolved.wrapper, ["workflow-wrapper"]);
    }

    #[test]
    fn resolved_settings_are_owned_and_wrapper_is_kept_as_argv() {
        let mut config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
wrapper = ["bin/wrapper", "; echo should-not-run"]
"#,
        );
        let resolved = config.resolve(context("Tines", "Build", "Implement"));
        config.wrapper.clear();
        config.overrides.clear();

        assert_eq!(resolved.wrapper, ["bin/wrapper", "; echo should-not-run"]);
    }

    #[test]
    fn defaults_and_tilde_expansion_are_predictable() {
        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                workspace_parent = "~/workspaces"
                [storage]
                credentials_file = "~/credentials/runner.toml"
            "#,
            defaults(),
        )
        .unwrap();

        assert_eq!(
            config.workspace_parent,
            PathBuf::from("/home/tester/workspaces")
        );
        assert_eq!(
            config.credentials_file,
            PathBuf::from("/home/tester/credentials/runner.toml")
        );
        assert_eq!(config.max_concurrent, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(15));
        assert_eq!(config.workspace_retention.mode, RetentionMode::Never);
        assert_eq!(
            config.workspace_retention.max_age,
            Duration::from_secs(72 * 60 * 60)
        );
        assert_eq!(config.workspace_retention.max_count, 20);
    }

    #[test]
    fn parses_all_runner_storage_and_retention_settings() {
        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://runner.example.test/api"

                [runner]
                name = "workstation"
                runner_type = "codex"
                workspace_parent = "~/runner-workspaces"
                wrapper = ["/usr/local/bin/codex-wrapper", "--trace"]
                max_concurrent = 4
                poll_interval_seconds = 9

                [storage]
                credentials_file = "~/.config/runner/credentials.toml"
                keep_workspaces = "failed"
                keep_workspaces_for_hours = 48
                keep_workspaces_max = 12
            "#,
            defaults(),
        )
        .unwrap();

        assert_eq!(
            config.server_url.as_str(),
            "https://runner.example.test/api"
        );
        assert_eq!(config.runner_name, "workstation");
        assert_eq!(config.runner_type, RunnerType::Codex);
        assert_eq!(
            config.workspace_parent,
            PathBuf::from("/home/tester/runner-workspaces")
        );
        assert_eq!(config.wrapper, ["/usr/local/bin/codex-wrapper", "--trace"]);
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.poll_interval, Duration::from_secs(9));
        assert_eq!(
            config.credentials_file,
            PathBuf::from("/home/tester/.config/runner/credentials.toml")
        );
        assert_eq!(config.workspace_retention.mode, RetentionMode::Failed);
        assert_eq!(
            config.workspace_retention.max_age,
            Duration::from_secs(48 * 60 * 60)
        );
        assert_eq!(config.workspace_retention.max_count, 12);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_paths_override_linux_home_defaults() {
        let home = PathBuf::from("/home/tester");
        let defaults = default_paths_for(
            home,
            Some(PathBuf::from("/custom/config")),
            Some(PathBuf::from("/custom/data")),
            None,
            None,
        );

        assert_eq!(
            defaults.config_dir,
            PathBuf::from("/custom/config/tines-runner-rs")
        );
        assert_eq!(
            defaults.credentials_file,
            PathBuf::from("/custom/config/tines-runner-rs/credentials.toml")
        );
        assert_eq!(
            defaults.workspace_parent,
            PathBuf::from("/custom/data/tines-runner-rs/workspaces")
        );

        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
            "#,
            defaults.clone(),
        )
        .unwrap();
        assert_eq!(config.workspace_parent, defaults.workspace_parent);
        assert_eq!(config.credentials_file, defaults.credentials_file);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn relative_xdg_paths_fall_back_to_linux_home_defaults() {
        let home = PathBuf::from("/home/tester");
        let defaults = default_paths_for(
            home.clone(),
            Some(PathBuf::from("relative/config")),
            Some(PathBuf::from("relative/data")),
            None,
            None,
        );

        assert_eq!(defaults.config_dir, home.join(".config/tines-runner-rs"));
        assert_eq!(
            defaults.workspace_parent,
            home.join(".local/share/tines-runner-rs/workspaces")
        );
    }

    #[test]
    fn non_codex_runner_types_and_invalid_limits_are_rejected() {
        let non_codex = r#"
            [server]
            url = "https://tines.example.test"
            [runner]
            name = "test-runner"
            runner_type = "claude"
        "#;
        assert!(Config::from_toml_str_with_defaults(non_codex, defaults()).is_err());

        let zero_concurrency = r#"
            [server]
            url = "https://tines.example.test"
            [runner]
            name = "test-runner"
            max_concurrent = 0
        "#;
        assert!(Config::from_toml_str_with_defaults(zero_concurrency, defaults()).is_err());
    }
}
