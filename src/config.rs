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
/// Add variants here when the runner gains another harness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunnerType {
    Codex,
    Custom,
}

/// Whether a completed assignment's workspace should be retained.
#[derive(Clone, Copy, Debug, Default, Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RetentionMode {
    #[default]
    Never,
    Failed,
    Always,
}

/// Whether the executor should create working trees for assigned repositories.
#[derive(Clone, Copy, Debug, Default, Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryCheckoutPolicy {
    /// Clone each assigned repository into the workspace.
    #[default]
    Enabled,
    /// Write repository metadata without creating working trees.
    MetadataOnly,
}

/// How the daemon delivers an assignment run key to its executor transport.
#[derive(Clone, Copy, Debug, Default, Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunKeyDelivery {
    /// Keep the run key in the JSON request written to executor stdin.
    #[default]
    Request,
    /// Set the run key on the executor transport process environment.
    Environment,
}

/// Bounds and mode for retained assignment workspaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRetention {
    pub mode: RetentionMode,
    pub max_age: Duration,
    pub max_count: usize,
}

/// Non-secret runner configuration after parsing and daemon-side path expansion.
#[derive(Clone, Debug)]
pub struct Config {
    pub server_url: Url,
    pub runner_name: String,
    pub runner_type: RunnerType,
    /// Executor workspace parent configured for new runs. `None` uses the
    /// executor environment's platform/XDG default.
    pub workspace_parent: Option<PathBuf>,
    /// Roots used only to recover workspace state written by older daemons.
    legacy_workspace_roots: Vec<PathBuf>,
    /// The argv prefix used to reach the local or isolated executor.
    pub executor: Vec<String>,
    /// Optional argv prefix used only for capability discovery. When absent,
    /// capability discovery uses the resolved executor command.
    pub capabilities_executor: Option<Vec<String>>,
    /// Default argv command for assignments using the custom harness.
    pub custom_command: Option<Vec<String>>,
    pub repository_checkout: RepositoryCheckoutPolicy,
    pub run_key_delivery: RunKeyDelivery,
    /// The daemon-side working directory for the executor transport process.
    pub executor_cwd: PathBuf,
    pub max_concurrent: usize,
    pub allow_remote_concurrency: bool,
    pub poll_interval: Duration,
    pub credentials_file: PathBuf,
    pub workspace_retention: WorkspaceRetention,
    overrides: Vec<ConfigOverride>,
}

/// The immutable execution settings resolved for one assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRunConfig {
    pub runner_type: RunnerType,
    /// Executor workspace parent configured for this assignment. `None` uses
    /// the executor environment's platform/XDG default.
    pub workspace_parent: Option<PathBuf>,
    /// The argv prefix used to reach the executor; `execute` is appended.
    pub executor: Vec<String>,
    /// Optional argv prefix used only for capability discovery. When absent,
    /// capability discovery uses this assignment's resolved executor command.
    pub capabilities_executor: Option<Vec<String>>,
    /// Command argv for the semantic custom harness, if configured.
    pub custom_command: Option<Vec<String>>,
    /// Repository materialization mode selected for this assignment.
    pub repository_checkout: RepositoryCheckoutPolicy,
    /// How this assignment's Tines run key crosses the daemon/executor boundary.
    pub run_key_delivery: RunKeyDelivery,
    /// Absolute daemon-side working directory for the executor process.
    pub executor_cwd: PathBuf,
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
    Parse {
        path: Option<PathBuf>,
        source: toml::de::Error,
    },
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "could not read config file {}: {source}", path.display())
            }
            Self::Parse {
                path: Some(path),
                source,
            } => write!(f, "invalid config file {}: {source}", path.display()),
            Self::Parse { path: None, source } => write!(f, "invalid config TOML: {source}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

impl Config {
    /// Return workspace roots needed to recover state from older daemons.
    pub fn legacy_workspace_roots(&self) -> Vec<PathBuf> {
        self.legacy_workspace_roots.clone()
    }

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
        Self::from_toml_str(&contents).map_err(|error| match error {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: Some(path.to_path_buf()),
                source,
            },
            error => error,
        })
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
        self.resolve_with_matches(context).config
    }

    /// Resolve all matching overrides and return their zero-based declaration
    /// indexes for diagnostics.
    pub fn resolve_with_matches(&self, context: MatchContext<'_>) -> ConfigResolution {
        let mut resolved = ResolvedRunConfig {
            runner_type: self.runner_type,
            workspace_parent: self.workspace_parent.clone(),
            executor: self.executor.clone(),
            capabilities_executor: self.capabilities_executor.clone(),
            custom_command: self.custom_command.clone(),
            repository_checkout: self.repository_checkout,
            run_key_delivery: self.run_key_delivery,
            executor_cwd: self.executor_cwd.clone(),
        };
        let mut matching_overrides = Vec::new();

        for (index, rule) in self.overrides.iter().enumerate() {
            if !rule.matches(context) {
                continue;
            }
            matching_overrides.push(index);
            if let Some(runner_type) = rule.runner_type {
                resolved.runner_type = runner_type;
            }
            if let Some(repository_checkout) = rule.repository_checkout {
                resolved.repository_checkout = repository_checkout;
            }
            if let Some(run_key_delivery) = rule.run_key_delivery {
                resolved.run_key_delivery = run_key_delivery;
            }
            if let Some(workspace_parent) = &rule.workspace_parent {
                resolved.workspace_parent = Some(workspace_parent.clone());
            }
            if let Some(executor) = &rule.executor {
                resolved.executor.clone_from(executor);
            }
            if let Some(capabilities_executor) = &rule.capabilities_executor {
                resolved
                    .capabilities_executor
                    .clone_from(&Some(capabilities_executor.clone()));
            }
            if let Some(custom_command) = &rule.custom_command {
                resolved
                    .custom_command
                    .clone_from(&Some(custom_command.clone()));
            }
            if let Some(executor_cwd) = &rule.executor_cwd {
                resolved.executor_cwd.clone_from(executor_cwd);
            }
        }

        ConfigResolution {
            config: resolved,
            matching_overrides,
        }
    }

    fn from_toml_str_with_defaults(
        contents: &str,
        defaults: DefaultPaths,
    ) -> Result<Self, ConfigError> {
        let raw: RawConfig =
            toml::from_str(contents).map_err(|source| ConfigError::Parse { path: None, source })?;

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
        let legacy_workspace_parent = expand_path(
            raw.runner
                .workspace_parent
                .as_deref()
                .unwrap_or(&defaults.workspace_parent),
            &defaults.home,
        )?;
        let workspace_parent = raw.runner.workspace_parent.clone();
        let mut legacy_workspace_roots = vec![legacy_workspace_parent];
        for workspace_parent in raw
            .overrides
            .iter()
            .filter_map(|rule| rule.workspace_parent.as_deref())
        {
            let workspace_parent = expand_path(workspace_parent, &defaults.home)?;
            if !legacy_workspace_roots.contains(&workspace_parent) {
                legacy_workspace_roots.push(workspace_parent);
            }
        }
        let executor_cwd = resolve_executor_cwd(
            raw.runner.executor_cwd.as_deref().ok_or_else(|| {
                ConfigError::Invalid(
                    "missing required [runner].executor_cwd; set the daemon-side working directory for the executor transport"
                        .to_owned(),
                )
            })?,
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
                    && rule.executor.is_none()
                    && rule.capabilities_executor.is_none()
                    && rule.executor_cwd.is_none()
                    && rule.custom_command.is_none()
                    && rule.repository_checkout.is_none()
                    && rule.run_key_delivery.is_none()
                {
                    return Err(ConfigError::Invalid(
                        "each [[override]] must set at least one override value".to_owned(),
                    ));
                }
                if let Some(command) = &rule.custom_command {
                    validate_custom_command(command)?;
                }
                Ok(ConfigOverride {
                    project: rule.project,
                    workflow: rule.workflow,
                    state: rule.state,
                    workspace_parent: rule.workspace_parent,
                    runner_type: rule.runner_type,
                    executor: rule.executor,
                    capabilities_executor: rule.capabilities_executor,
                    custom_command: rule.custom_command,
                    repository_checkout: rule.repository_checkout,
                    run_key_delivery: rule.run_key_delivery,
                    executor_cwd: rule
                        .executor_cwd
                        .as_deref()
                        .map(|path| resolve_executor_cwd(path, &defaults.home))
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;

        Ok(Self {
            server_url,
            runner_name,
            runner_type: raw.runner.runner_type.unwrap_or(RunnerType::Codex),
            workspace_parent,
            legacy_workspace_roots,
            executor: raw
                .runner
                .executor
                .unwrap_or_else(|| vec!["tines-runner-rs".to_owned()]),
            capabilities_executor: raw.runner.capabilities_executor,
            custom_command: raw
                .runner
                .custom_command
                .map(|command| validate_custom_command(&command).map(|()| command))
                .transpose()?,
            repository_checkout: raw.runner.repository_checkout.unwrap_or_default(),
            run_key_delivery: raw.runner.run_key_delivery.unwrap_or_default(),
            executor_cwd,
            max_concurrent,
            allow_remote_concurrency: raw.runner.allow_remote_concurrency,
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

/// Effective settings and the override entries that matched one assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigResolution {
    pub config: ResolvedRunConfig,
    matching_overrides: Vec<usize>,
}

impl ConfigResolution {
    /// Zero-based positions in the `[[override]]` list, in declaration order.
    pub fn matching_overrides(&self) -> &[usize] {
        &self.matching_overrides
    }
}

#[derive(Clone, Debug)]
struct ConfigOverride {
    project: Option<String>,
    workflow: Option<String>,
    state: Option<String>,
    workspace_parent: Option<PathBuf>,
    runner_type: Option<RunnerType>,
    executor: Option<Vec<String>>,
    capabilities_executor: Option<Vec<String>>,
    custom_command: Option<Vec<String>>,
    repository_checkout: Option<RepositoryCheckoutPolicy>,
    run_key_delivery: Option<RunKeyDelivery>,
    executor_cwd: Option<PathBuf>,
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
    executor: Option<Vec<String>>,
    capabilities_executor: Option<Vec<String>>,
    custom_command: Option<Vec<String>>,
    repository_checkout: Option<RepositoryCheckoutPolicy>,
    run_key_delivery: Option<RunKeyDelivery>,
    executor_cwd: Option<PathBuf>,
    max_concurrent: Option<usize>,
    #[serde(default)]
    allow_remote_concurrency: bool,
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
    executor: Option<Vec<String>>,
    capabilities_executor: Option<Vec<String>>,
    custom_command: Option<Vec<String>>,
    repository_checkout: Option<RepositoryCheckoutPolicy>,
    run_key_delivery: Option<RunKeyDelivery>,
    executor_cwd: Option<PathBuf>,
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

/// Resolve a workspace path in the current executor environment.
///
/// An absent path selects the executor's platform/XDG default. A configured
/// path is kept unexpanded by the daemon so that `~` resolves against the
/// executor's home directory.
pub fn resolve_workspace_parent(path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = path
        && path != Path::new("~")
        && path.strip_prefix("~").is_err()
    {
        return Ok(path.to_path_buf());
    }
    let defaults = default_paths()?;
    resolve_workspace_parent_with_defaults(path, &defaults)
}

fn resolve_workspace_parent_with_defaults(
    path: Option<&Path>,
    defaults: &DefaultPaths,
) -> Result<PathBuf, ConfigError> {
    expand_path(path.unwrap_or(&defaults.workspace_parent), &defaults.home)
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
        home: home.clone(),
        credentials_file: config_dir.join("credentials.toml"),
        config_dir,
        workspace_parent: data_dir.join("tines-runner-rs").join("workspaces"),
    }
}

fn resolve_executor_cwd(path: &Path, home: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid(
            "[runner].executor_cwd must not be empty".to_owned(),
        ));
    }
    let expanded = expand_path(path, home)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(home.join(expanded))
    }
}

fn validate_custom_command(command: &[String]) -> Result<(), ConfigError> {
    if command.is_empty() || command[0].trim().is_empty() {
        return Err(ConfigError::Invalid(
            "custom_command must contain a non-empty executable argument".to_owned(),
        ));
    }
    if command.iter().any(|argument| argument.contains('\0')) {
        return Err(ConfigError::Invalid(
            "custom_command arguments must not contain null bytes".to_owned(),
        ));
    }
    Ok(())
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
                executor_cwd = "/host/default"

                {overrides}
            "#
        );
        Config::from_toml_str_with_defaults(&contents, defaults()).unwrap()
    }

    #[test]
    fn legacy_workspace_roots_include_distinct_override_roots() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
workspace_parent = "/project/workspaces"

[[override]]
workflow = "Implementation"
workspace_parent = "/review/workspaces"

[[override]]
state = "Review"
workspace_parent = "/project/workspaces"
"#,
        );

        assert_eq!(
            config.legacy_workspace_roots(),
            [
                PathBuf::from("/default/workspaces"),
                PathBuf::from("/project/workspaces"),
                PathBuf::from("/review/workspaces"),
            ]
        );
    }

    #[test]
    fn executor_workspace_paths_keep_tildes_and_use_executor_defaults() {
        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
workspace_parent = "~/work/base"
executor_cwd = "/daemon/transport"
[[override]]
project = "Payments"
workspace_parent = "~/work/payments"
"#,
            defaults(),
        )
        .unwrap();

        assert_eq!(config.workspace_parent, Some(PathBuf::from("~/work/base")));
        assert_eq!(
            config.legacy_workspace_roots(),
            [
                PathBuf::from("/home/tester/work/base"),
                PathBuf::from("/home/tester/work/payments"),
            ]
        );
        let resolved = config.resolve(context("Payments", "Build", "Ready"));
        assert_eq!(
            resolved.workspace_parent,
            Some(PathBuf::from("~/work/payments"))
        );

        let executor_defaults = default_paths_for(
            PathBuf::from("/executor/home"),
            None,
            Some(PathBuf::from("/executor/xdg-data")),
            None,
            None,
        );
        assert_eq!(
            resolve_workspace_parent_with_defaults(
                config.workspace_parent.as_deref(),
                &executor_defaults,
            )
            .unwrap(),
            PathBuf::from("/executor/home/work/base")
        );
        assert_eq!(
            resolve_workspace_parent_with_defaults(
                resolved.workspace_parent.as_deref(),
                &executor_defaults,
            )
            .unwrap(),
            PathBuf::from("/executor/home/work/payments")
        );

        let default_config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = "/daemon/transport"
"#,
            defaults(),
        )
        .unwrap();
        assert_eq!(default_config.workspace_parent, None);
        assert_eq!(
            resolve_workspace_parent_with_defaults(None, &executor_defaults).unwrap(),
            PathBuf::from("/executor/xdg-data/tines-runner-rs/workspaces")
        );
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
            Some(PathBuf::from("/project"))
        );
        assert_eq!(
            config
                .resolve(context("Tines Tools", "Build", "Implement"))
                .workspace_parent,
            Some(PathBuf::from("/default/workspaces"))
        );
    }

    #[test]
    fn workflow_only_override_matches_by_workflow() {
        let config = parse_with_overrides(
            r#"[[override]]
workflow = "Implementation"
executor = ["implementation-executor"]
"#,
        );

        assert_eq!(
            config
                .resolve(context("Other", "implementation", "Review"))
                .executor,
            ["implementation-executor"]
        );
        assert_eq!(
            config
                .resolve(context("Other", "Implement", "Review"))
                .executor,
            ["tines-runner-rs"]
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
executor = ["review-executor"]
"#,
        );

        assert_eq!(
            config
                .resolve(context("tines", "implementation", "review"))
                .executor,
            ["review-executor"]
        );
        assert_eq!(
            config
                .resolve(context("Tines", "Implementation", "Implement"))
                .executor,
            ["tines-runner-rs"]
        );
    }

    #[test]
    fn overlapping_overrides_apply_in_declaration_order_by_field() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
workspace_parent = "/project"
executor = ["project-executor"]

[[override]]
workflow = "Implementation"
executor = ["workflow-executor"]

[[override]]
state = "Review"
workspace_parent = "/review"
"#,
        );

        let resolved = config.resolve(context("Tines", "Implementation", "Review"));
        assert_eq!(resolved.workspace_parent, Some(PathBuf::from("/review")));
        assert_eq!(resolved.executor, ["workflow-executor"]);
    }

    #[test]
    fn repository_checkout_policy_defaults_to_enabled_and_overrides_by_selector() {
        let default_config = parse_with_overrides("");
        assert_eq!(
            default_config
                .resolve(context("Other", "Build", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::Enabled
        );

        let config = parse_with_overrides(
            r#"repository_checkout = "metadata_only"

[[override]]
project = "Tines"
repository_checkout = "enabled"

[[override]]
workflow = "Implementation"
repository_checkout = "enabled"

[[override]]
state = "Review"
repository_checkout = "enabled"
"#,
        );

        assert_eq!(
            config
                .resolve(context("Other", "Build", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::MetadataOnly
        );
        assert_eq!(
            config
                .resolve(context("Tines", "Build", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::Enabled
        );
        assert_eq!(
            config
                .resolve(context("Other", "Implementation", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::Enabled
        );
        assert_eq!(
            config
                .resolve(context("Other", "Build", "Review"))
                .repository_checkout,
            RepositoryCheckoutPolicy::Enabled
        );
    }

    #[test]
    fn run_key_delivery_defaults_to_request_and_can_be_selected_by_each_override_kind() {
        let defaults = parse_with_overrides("");
        assert_eq!(
            defaults
                .resolve(context("Other", "Build", "Ready"))
                .run_key_delivery,
            RunKeyDelivery::Request
        );

        let config = parse_with_overrides(
            r#"run_key_delivery = "request"

[[override]]
project = "Project"
run_key_delivery = "environment"

[[override]]
workflow = "Workflow"
run_key_delivery = "environment"

[[override]]
state = "State"
run_key_delivery = "environment"
"#,
        );

        for context in [
            context("Project", "Other", "Other"),
            context("Other", "Workflow", "Other"),
            context("Other", "Other", "State"),
        ] {
            assert_eq!(
                config.resolve(context).run_key_delivery,
                RunKeyDelivery::Environment
            );
        }
        assert_eq!(
            config
                .resolve(context("Other", "Other", "Other"))
                .run_key_delivery,
            RunKeyDelivery::Request
        );
    }

    #[test]
    fn custom_commands_resolve_by_field_in_override_declaration_order() {
        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"

[runner]
name = "test-runner"
executor_cwd = "/host/default"
runner_type = "custom"
custom_command = ["base-checks", "{prompt_file}"]

[[override]]
project = "Payments"
custom_command = ["payments-checks", "{workspace}"]

[[override]]
state = "Review"
custom_command = ["review-checks", "{prompt_file}"]
"#,
            defaults(),
        )
        .unwrap();

        let payments = config.resolve(context("Payments", "Build", "Implement"));
        assert_eq!(payments.runner_type, RunnerType::Custom);
        assert_eq!(
            payments.custom_command,
            Some(vec!["payments-checks".to_owned(), "{workspace}".to_owned()])
        );

        let review = config.resolve(context("Other", "Build", "Review"));
        assert_eq!(
            review.custom_command,
            Some(vec!["review-checks".to_owned(), "{prompt_file}".to_owned()])
        );

        let overlapping = config.resolve(context("Payments", "Build", "Review"));
        assert_eq!(
            overlapping.custom_command,
            Some(vec!["review-checks".to_owned(), "{prompt_file}".to_owned()])
        );
    }

    #[test]
    fn custom_commands_must_have_a_program_and_no_null_bytes() {
        for command in ["[]", "[\"\"]", "[\"check\", \"bad\\u0000arg\"]"] {
            let config = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"test\"\nrunner_type = \"custom\"\ncustom_command = {command}\nexecutor_cwd = \"/daemon\"\n"
            );
            assert!(Config::from_toml_str_with_defaults(&config, defaults()).is_err());
        }
    }

    #[test]
    fn resolved_executor_settings_are_owned_and_kept_as_argv() {
        let mut config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
executor = ["bin/executor", "; literal argument"]
"#,
        );
        let resolved = config.resolve(context("Tines", "Build", "Implement"));
        config.executor.clear();
        config.overrides.clear();

        assert_eq!(resolved.executor, ["bin/executor", "; literal argument"]);
    }

    #[test]
    fn executor_command_and_daemon_cwd_resolve_with_project_override() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Tines"
executor = ["docker", "run", "--rm", "-i", "runner-image"]
executor_cwd = "~/executor"
"#,
        );

        let resolved = config.resolve(context("tines", "Build", "Implement"));
        assert_eq!(
            resolved.executor,
            ["docker", "run", "--rm", "-i", "runner-image"]
        );
        assert_eq!(
            resolved.executor_cwd,
            PathBuf::from("/home/tester/executor")
        );

        let fallback = config.resolve(context("Other", "Build", "Implement"));
        assert_eq!(fallback.executor, ["tines-runner-rs"]);
        assert_eq!(fallback.executor_cwd, PathBuf::from("/host/default"));
    }

    #[test]
    fn executor_argv_is_not_interpreted_by_configuration() {
        for executor in [
            r#"["docker", "run", "--rm", "-dit", "runner-image"]"#,
            r#"["podman", "run", "--detach", "runner-image"]"#,
            r#"["custom-transport", "run", "-d"]"#,
        ] {
            let contents = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"test-runner\"\nexecutor_cwd = \"/daemon\"\nexecutor = {executor}\n"
            );
            Config::from_toml_str_with_defaults(&contents, defaults())
                .expect("executor argv is an opaque transport command");
        }

        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = "/daemon"
[[override]]
project = "Payments"
executor = ["docker", "run", "--detach", "runner-image"]
"#,
            defaults(),
        )
        .expect("override executor argv is also opaque");
        assert_eq!(
            config
                .resolve(context("Payments", "Build", "Implement"))
                .executor,
            ["docker", "run", "--detach", "runner-image"]
        );
    }

    #[test]
    fn executor_and_daemon_cwd_resolve_by_project_workflow_and_state() {
        let config = parse_with_overrides(
            r#"[[override]]
project = "Payments"
executor = ["docker", "run", "--rm", "-i", "payments-image"]
executor_cwd = "/host/payments"

[[override]]
workflow = "Implementation"
executor = ["podman", "run", "--rm", "-i", "implementation-image"]
executor_cwd = "~/implementation"

[[override]]
state = "Review"
executor = ["docker", "run", "--rm", "-i", "review-image"]
executor_cwd = "review"
"#,
        );

        let project = config.resolve(context("payments", "Build", "Ready"));
        assert_eq!(project.executor[0], "docker");
        assert_eq!(project.executor_cwd, PathBuf::from("/host/payments"));

        let workflow = config.resolve(context("Other", "implementation", "Ready"));
        assert_eq!(workflow.executor[0], "podman");
        assert_eq!(
            workflow.executor_cwd,
            PathBuf::from("/home/tester/implementation")
        );

        let state = config.resolve(context("Other", "Build", "review"));
        assert_eq!(state.executor[0], "docker");
        assert_eq!(state.executor_cwd, PathBuf::from("/home/tester/review"));

        let fallback = config.resolve(context("Other", "Build", "Ready"));
        assert_eq!(fallback.executor, ["tines-runner-rs"]);
        assert_eq!(fallback.executor_cwd, PathBuf::from("/host/default"));
    }

    #[test]
    fn capabilities_executor_resolves_by_field_in_override_declaration_order() {
        let config = parse_with_overrides(
            r#"executor = ["default-executor"]
capabilities_executor = ["default-capabilities"]

[[override]]
project = "Payments"
executor = ["payments-executor"]
capabilities_executor = ["payments-capabilities"]

[[override]]
workflow = "Implementation"
capabilities_executor = ["implementation-capabilities"]

[[override]]
state = "Review"
capabilities_executor = ["review-capabilities"]
"#,
        );

        assert_eq!(
            config
                .resolve(context("Payments", "Build", "Ready"))
                .executor,
            ["payments-executor"]
        );
        assert_eq!(
            config
                .resolve(context("Payments", "Build", "Ready"))
                .capabilities_executor,
            Some(vec!["payments-capabilities".to_owned()])
        );
        assert_eq!(
            config
                .resolve(context("Other", "Implementation", "Ready"))
                .capabilities_executor,
            Some(vec!["implementation-capabilities".to_owned()])
        );
        assert_eq!(
            config
                .resolve(context("Other", "Build", "Review"))
                .capabilities_executor,
            Some(vec!["review-capabilities".to_owned()])
        );
        assert_eq!(
            config
                .resolve(context("Payments", "Implementation", "Review"))
                .capabilities_executor,
            Some(vec!["review-capabilities".to_owned()])
        );
    }

    #[test]
    fn capabilities_executor_falls_back_to_the_resolved_executor_when_absent() {
        let config = parse_with_overrides(
            r#"executor = ["default-executor"]

[[override]]
state = "Review"
executor = ["review-executor"]
"#,
        );

        let resolved = config.resolve(context("Other", "Build", "Review"));
        assert_eq!(resolved.executor, ["review-executor"]);
        assert_eq!(resolved.capabilities_executor, None);
    }

    #[test]
    fn explicit_executor_cwd_resolves_under_home_and_empty_is_rejected() {
        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = "executor"
"#,
            defaults(),
        )
        .unwrap();
        assert_eq!(config.executor_cwd, PathBuf::from("/home/tester/executor"));

        let error = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = ""
"#,
            defaults(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("executor_cwd must not be empty"));
    }

    #[test]
    fn executor_cwd_is_required_without_an_implicit_default() {
        let error = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
"#,
            defaults(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required [runner].executor_cwd")
        );
    }

    #[test]
    fn native_executor_uses_explicit_daemon_cwd() {
        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "native-runner"
executor_cwd = "/srv/tines-runner"
"#,
            defaults(),
        )
        .unwrap();
        assert_eq!(config.executor, ["tines-runner-rs"]);
        assert_eq!(config.executor_cwd, PathBuf::from("/srv/tines-runner"));
    }

    #[test]
    fn defaults_and_tilde_expansion_are_predictable() {
        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                executor_cwd = "~/daemon"
                workspace_parent = "~/workspaces"
                [storage]
                credentials_file = "~/credentials/runner.toml"
            "#,
            defaults(),
        )
        .unwrap();

        assert_eq!(config.workspace_parent, Some(PathBuf::from("~/workspaces")));
        assert_eq!(
            config.legacy_workspace_roots(),
            [PathBuf::from("/home/tester/workspaces")]
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
                executor_cwd = "~/daemon"
                max_concurrent = 4
                poll_interval_seconds = 9
                allow_remote_concurrency = true

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
            Some(PathBuf::from("~/runner-workspaces"))
        );
        assert_eq!(config.executor_cwd, PathBuf::from("/home/tester/daemon"));
        assert_eq!(config.max_concurrent, 4);
        assert!(config.allow_remote_concurrency);
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
                executor_cwd = "~/daemon"
            "#,
            defaults.clone(),
        )
        .unwrap();
        assert_eq!(config.legacy_workspace_roots(), [defaults.workspace_parent]);
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
            executor_cwd = "~/daemon"
            runner_type = "claude"
        "#;
        assert!(Config::from_toml_str_with_defaults(non_codex, defaults()).is_err());

        let zero_concurrency = r#"
            [server]
            url = "https://tines.example.test"
            [runner]
            name = "test-runner"
            executor_cwd = "~/daemon"
            max_concurrent = 0
        "#;
        assert!(Config::from_toml_str_with_defaults(zero_concurrency, defaults()).is_err());
    }

    #[test]
    fn legacy_wrapper_settings_are_rejected_after_executor_migration() {
        for legacy_setting in [
            "wrapper = [\"codex-wrapper\"]",
            "[[override]]\nproject = \"Tines\"\nwrapper = [\"codex-wrapper\"]",
        ] {
            let config = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"test-runner\"\nexecutor_cwd = \"~/daemon\"\n{legacy_setting}\n"
            );
            assert!(
                Config::from_toml_str_with_defaults(&config, defaults()).is_err(),
                "legacy setting was accepted: {legacy_setting}"
            );
        }
    }
}
