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
#[derive(Clone, Copy, Debug, Default, Deserialize, serde::Serialize, Eq, PartialEq)]
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

/// Non-secret runner configuration after parsing and daemon-side path expansion.
#[derive(Clone, Debug)]
pub struct Config {
    pub server_url: Url,
    pub runner_name: String,
    pub runner_type: RunnerType,
    pub workspace_parent: PathBuf,
    /// Unexpanded workspace path for the executor environment. `None` asks
    /// the executor to use its platform/XDG default.
    pub executor_workspace_parent: Option<PathBuf>,
    /// Deprecated argv prefix for the legacy direct-Codex execution path.
    pub wrapper: Vec<String>,
    /// The argv prefix used to reach the local or isolated executor.
    pub executor: Vec<String>,
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
    pub workspace_parent: PathBuf,
    /// Unexpanded workspace path for the executor environment. `None` asks
    /// the executor to use its platform/XDG default.
    pub executor_workspace_parent: Option<PathBuf>,
    /// Deprecated argv prefix for the legacy direct-Codex execution path.
    pub wrapper: Vec<String>,
    /// The argv prefix used to reach the executor; `execute` is appended.
    pub executor: Vec<String>,
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
    /// Return each distinct workspace parent configured by the default or an override.
    pub fn workspace_parents(&self) -> Vec<PathBuf> {
        let mut parents = vec![self.workspace_parent.clone()];
        for parent in self
            .overrides
            .iter()
            .filter_map(|rule| rule.workspace_parent.as_ref())
        {
            if !parents.contains(parent) {
                parents.push(parent.clone());
            }
        }
        parents
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
            executor_workspace_parent: self.executor_workspace_parent.clone(),
            wrapper: self.wrapper.clone(),
            executor: self.executor.clone(),
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
            if let Some(workspace_parent) = &rule.workspace_parent {
                resolved.workspace_parent.clone_from(workspace_parent);
            }
            if let Some(workspace_parent) = &rule.executor_workspace_parent {
                resolved.executor_workspace_parent = Some(workspace_parent.clone());
            }
            if let Some(wrapper) = &rule.wrapper {
                resolved.wrapper.clone_from(wrapper);
            }
            if let Some(executor) = &rule.executor {
                resolved.executor.clone_from(executor);
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
        if let Some(executor) = raw.runner.executor.as_deref() {
            validate_container_executor(executor)?;
        }
        for rule in &raw.overrides {
            if let Some(executor) = rule.executor.as_deref() {
                validate_container_executor(executor)?;
            }
        }
        if raw.runner.wrapper.is_some() {
            tracing::warn!(
                "[runner].wrapper is deprecated and applies only to the legacy direct-Codex path; configure the executor environment instead"
            );
        }
        for (index, rule) in raw.overrides.iter().enumerate() {
            if rule.wrapper.is_some() {
                tracing::warn!(
                    override_index = index + 1,
                    "[[override]].wrapper is deprecated and applies only to the legacy direct-Codex path; configure the executor environment instead"
                );
            }
        }
        let workspace_parent = expand_path(
            raw.runner
                .workspace_parent
                .as_deref()
                .unwrap_or(&defaults.workspace_parent),
            &defaults.home,
        )?;
        let executor_workspace_parent = raw.runner.workspace_parent.clone();
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
                    && rule.wrapper.is_none()
                    && rule.executor.is_none()
                    && rule.executor_cwd.is_none()
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
                    executor_workspace_parent: rule.workspace_parent,
                    runner_type: rule.runner_type,
                    wrapper: rule.wrapper,
                    executor: rule.executor,
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
            executor_workspace_parent,
            wrapper: raw.runner.wrapper.unwrap_or_default(),
            executor: raw
                .runner
                .executor
                .unwrap_or_else(|| vec!["tines-runner-rs".to_owned()]),
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

fn validate_container_executor(argv: &[String]) -> Result<(), ConfigError> {
    let Some(program) = argv
        .first()
        .and_then(|program| Path::new(program).file_name())
    else {
        return Ok(());
    };
    let program = program.to_string_lossy().to_ascii_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program);
    if !matches!(program, "docker" | "podman") {
        return Ok(());
    }

    let Some(run_index) = argv.iter().position(|argument| argument == "run") else {
        return Ok(());
    };
    if container_run_detaches(&argv[run_index + 1..]) {
        return Err(ConfigError::Invalid(
            "docker and podman executor commands must run containers in the foreground; remove -d or --detach so the daemon can supervise container termination".to_owned(),
        ));
    }
    Ok(())
}

/// Inspect only Docker/Podman run options before the image positional argument.
fn container_run_detaches(arguments: &[String]) -> bool {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            break;
        }
        if argument == "--detach" || argument == "--detach=true" || argument == "-d=true" {
            return true;
        }
        if let Some(value) = argument.strip_prefix("--detach=") {
            if value != "false" {
                return true;
            }
            index += 1;
            continue;
        }
        if argument == "-d=false" {
            index += 1;
            continue;
        }
        if argument.starts_with("--") {
            let name = argument
                .split_once('=')
                .map_or(argument.as_str(), |(name, _)| name);
            index +=
                usize::from(!argument.contains('=') && container_long_option_takes_value(name));
            index += 1;
            continue;
        }
        if argument.starts_with('-') && argument.len() > 1 {
            let mut chars = argument[1..].chars();
            while let Some(short_option) = chars.next() {
                if short_option == 'd' {
                    return true;
                }
                if container_short_option_takes_value(short_option) {
                    if chars.next().is_none() {
                        index += 1;
                    }
                    break;
                }
            }
            index += 1;
            continue;
        }

        // Docker and Podman interpret the first positional argument as IMAGE.
        // Later arguments belong to the image's command and must not be read
        // as transport options.
        break;
    }
    false
}

fn container_long_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--add-host"
            | "--annotation"
            | "--arch"
            | "--attach"
            | "--authfile"
            | "--blkio-weight"
            | "--blkio-weight-device"
            | "--cap-add"
            | "--cap-drop"
            | "--cgroup-conf"
            | "--cgroup-parent"
            | "--cgroups"
            | "--cgroupns"
            | "--cidfile"
            | "--conmon-pidfile"
            | "--cpu-period"
            | "--cpu-quota"
            | "--cpu-rt-period"
            | "--cpu-rt-runtime"
            | "--cpu-shares"
            | "--cpus"
            | "--cpuset-cpus"
            | "--cpuset-mems"
            | "--creds"
            | "--detach-keys"
            | "--device"
            | "--device-cgroup-rule"
            | "--device-read-bps"
            | "--device-read-iops"
            | "--device-write-bps"
            | "--device-write-iops"
            | "--dns"
            | "--dns-option"
            | "--dns-search"
            | "--domainname"
            | "--entrypoint"
            | "--env"
            | "--env-file"
            | "--env-merge"
            | "--expose"
            | "--gpus"
            | "--gidmap"
            | "--group-add"
            | "--health-cmd"
            | "--health-interval"
            | "--health-retries"
            | "--health-start-interval"
            | "--health-start-period"
            | "--health-timeout"
            | "--hostname"
            | "--image-volume"
            | "--init-path"
            | "--ip"
            | "--ip6"
            | "--ipc"
            | "--isolation"
            | "--kernel-memory"
            | "--label"
            | "--label-file"
            | "--link"
            | "--link-local-ip"
            | "--log-driver"
            | "--log-opt"
            | "--mac-address"
            | "--memory"
            | "--memory-reservation"
            | "--memory-swap"
            | "--memory-swappiness"
            | "--mount"
            | "--name"
            | "--net"
            | "--network"
            | "--network-alias"
            | "--oom-score-adj"
            | "--os"
            | "--pid"
            | "--pidfile"
            | "--pids-limit"
            | "--platform"
            | "--pod"
            | "--preserve-fds"
            | "--publish"
            | "--pull"
            | "--restart"
            | "--runtime"
            | "--security-opt"
            | "--seccomp-profile"
            | "--shm-size"
            | "--stop-signal"
            | "--stop-timeout"
            | "--storage-opt"
            | "--subgidname"
            | "--subuidname"
            | "--sysctl"
            | "--tmpfs"
            | "--uidmap"
            | "--ulimit"
            | "--unsetenv"
            | "--user"
            | "--userns"
            | "--uts"
            | "--volume"
            | "--volume-driver"
            | "--volumes-from"
            | "--workdir"
    )
}

fn container_short_option_takes_value(option: char) -> bool {
    matches!(
        option,
        'a' | 'c' | 'e' | 'h' | 'l' | 'm' | 'p' | 'u' | 'v' | 'w'
    )
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
    executor_workspace_parent: Option<PathBuf>,
    runner_type: Option<RunnerType>,
    wrapper: Option<Vec<String>>,
    executor: Option<Vec<String>>,
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
    wrapper: Option<Vec<String>>,
    executor: Option<Vec<String>>,
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
    wrapper: Option<Vec<String>>,
    executor: Option<Vec<String>>,
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
pub fn resolve_executor_workspace_parent(path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = path
        && path != Path::new("~")
        && path.strip_prefix("~").is_err()
    {
        return Ok(path.to_path_buf());
    }
    let defaults = default_paths()?;
    resolve_executor_workspace_parent_with_defaults(path, &defaults)
}

fn resolve_executor_workspace_parent_with_defaults(
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
                wrapper = ["base-wrapper", "--"]

                {overrides}
            "#
        );
        Config::from_toml_str_with_defaults(&contents, defaults()).unwrap()
    }

    #[test]
    fn workspace_parents_include_distinct_override_roots() {
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
            config.workspace_parents(),
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

        assert_eq!(
            config.workspace_parent,
            PathBuf::from("/home/tester/work/base")
        );
        assert_eq!(
            config.executor_workspace_parent,
            Some(PathBuf::from("~/work/base"))
        );
        let resolved = config.resolve(context("Payments", "Build", "Ready"));
        assert_eq!(
            resolved.workspace_parent,
            PathBuf::from("/home/tester/work/payments")
        );
        assert_eq!(
            resolved.executor_workspace_parent,
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
            resolve_executor_workspace_parent_with_defaults(
                config.executor_workspace_parent.as_deref(),
                &executor_defaults,
            )
            .unwrap(),
            PathBuf::from("/executor/home/work/base")
        );
        assert_eq!(
            resolve_executor_workspace_parent_with_defaults(
                resolved.executor_workspace_parent.as_deref(),
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
        assert_eq!(default_config.executor_workspace_parent, None);
        assert_eq!(
            resolve_executor_workspace_parent_with_defaults(None, &executor_defaults).unwrap(),
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
    fn docker_and_podman_executor_runs_must_remain_attached() {
        for executor in [
            r#"["docker", "run", "--rm", "-d", "runner-image"]"#,
            r#"["/usr/bin/podman", "run", "--detach=true", "runner-image"]"#,
            r#"["docker", "run", "--rm", "-dit", "runner-image"]"#,
            r#"["podman", "run", "-id", "--name", "runner", "runner-image"]"#,
        ] {
            let contents = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"test-runner\"\nexecutor_cwd = \"/daemon\"\nexecutor = {executor}\n"
            );
            let error = Config::from_toml_str_with_defaults(&contents, defaults())
                .expect_err("detached container executor must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("run containers in the foreground")
            );
        }

        let error = Config::from_toml_str_with_defaults(
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
        .expect_err("detached override must be rejected");
        assert!(
            error
                .to_string()
                .contains("run containers in the foreground")
        );
    }

    #[test]
    fn container_option_values_and_executor_arguments_are_not_detach_flags() {
        for executor in [
            r#"["docker", "run", "--env=-d", "runner-image", "tines-runner-rs", "execute", "-d"]"#,
            r#"["docker", "run", "--env", "-d", "runner-image"]"#,
            r#"["podman", "run", "--detach-keys", "-d", "runner-image"]"#,
        ] {
            let contents = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"test-runner\"\nexecutor_cwd = \"/daemon\"\nexecutor = {executor}\n"
            );
            Config::from_toml_str_with_defaults(&contents, defaults())
                .expect("option values and executor arguments are outside Docker run options");
        }

        let attached_false = r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = "/daemon"
executor = ["docker", "run", "--detach=false", "runner-image"]
"#;
        Config::from_toml_str_with_defaults(attached_false, defaults())
            .expect("an explicit false detach option keeps the container attached");
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
                executor_cwd = "~/daemon"
                wrapper = ["/usr/local/bin/codex-wrapper", "--trace"]
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
            PathBuf::from("/home/tester/runner-workspaces")
        );
        assert_eq!(config.executor_cwd, PathBuf::from("/home/tester/daemon"));
        assert_eq!(config.wrapper, ["/usr/local/bin/codex-wrapper", "--trace"]);
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
}
