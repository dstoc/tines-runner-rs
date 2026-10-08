//! TOML configuration and assignment-specific execution policy.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

/// The harness types supported by the runner.
///
/// Add variants here when the runner gains another harness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunnerType {
    Codex,
    Antigravity,
    Custom,
}

impl RunnerType {
    /// The semantic harness identifier used inside the executor.
    pub const fn executor_harness(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Custom => "custom",
        }
    }

    /// The harness identifier supported by the current Tines registration API.
    ///
    /// Tines does not expose a native Antigravity identity yet, so this maps
    /// Antigravity to the protocol compatibility alias `pi`. This alias does
    /// not mean that the runner implements Pi CLI behavior. Do not infer
    /// Antigravity continuation support from it. Any future resume support
    /// must define Antigravity semantics and opt in explicitly. Change this
    /// registration mapping and `tines_capability_harness` when Tines adds a
    /// native Antigravity identity.
    pub const fn tines_harness(self) -> crate::protocol::RunnerHarness {
        match self {
            Self::Codex => crate::protocol::RunnerHarness::Codex,
            Self::Antigravity => crate::protocol::RunnerHarness::Pi,
            Self::Custom => crate::protocol::RunnerHarness::Custom,
        }
    }

    /// The semantic harness identifier to look up in executor capabilities.
    pub const fn effort_capability_harness(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex"),
            Self::Antigravity => Some("antigravity"),
            Self::Custom => None,
        }
    }

    /// The harness identifier to advertise in Tines capability reports.
    ///
    /// For Antigravity, `pi` is only a Tines protocol alias. Keep execution
    /// and any future continuation decisions tied to the internal harness
    /// identity, with an explicit Antigravity opt-in for resume semantics.
    pub const fn tines_capability_harness(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex"),
            Self::Antigravity => Some("pi"),
            Self::Custom => None,
        }
    }

    /// Whether this runner has defined semantics for continuing a prior run.
    ///
    /// Generic resume behavior must use this internal-harness gate, not the
    /// Tines protocol identity returned by `tines_harness` or
    /// `tines_capability_harness`. In particular, the `pi` compatibility alias
    /// does not opt Antigravity into Pi continuation semantics. Enable a
    /// runner only after defining its continuation contract and adding tests.
    pub const fn supports_continuation(self) -> bool {
        match self {
            Self::Codex | Self::Antigravity | Self::Custom => false,
        }
    }
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
    /// Stable local identifier used to select this runner from a shared config.
    pub local_id: String,
    pub server_url: Url,
    pub runner_name: String,
    pub runner_type: RunnerType,
    /// Shared writable daemon state root. Each runner gets a separate,
    /// collision-safe namespace beneath it.
    pub state_dir: PathBuf,
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
    active_runs_file: PathBuf,
    legacy_active_runs_file: PathBuf,
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

impl ConfigError {
    fn with_path(self, path: &Path) -> Self {
        match self {
            Self::Parse { source, .. } => Self::Parse {
                path: Some(path.to_path_buf()),
                source,
            },
            error => error,
        }
    }
}

impl Config {
    /// Return workspace roots needed to recover state from older daemons.
    pub fn legacy_workspace_roots(&self) -> Vec<PathBuf> {
        self.legacy_workspace_roots.clone()
    }

    /// Return the persisted active-run state file for this selected runner.
    pub fn active_runs_file(&self) -> &Path {
        &self.active_runs_file
    }

    /// Return the pre-state-directory location, used only to import recovery
    /// records written by an earlier runner version.
    pub fn legacy_active_runs_file(&self) -> &Path {
        &self.legacy_active_runs_file
    }

    /// Load configuration from a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_for_runner(path, None)
    }

    /// Load the selected runner from a shared config file.
    pub fn load_for_runner(
        path: impl AsRef<Path>,
        runner_id: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut defaults = default_paths()?;
        defaults.config_dir = config_file_directory(path)?;
        Self::from_toml_str_with_defaults_and_runner(&contents, defaults, runner_id)
            .map_err(|error| error.with_path(path))
    }

    /// Load every named runner for daemon supervision, or one selected runner.
    /// A legacy `[runner]` configuration always produces one runner.
    pub fn load_for_daemon(
        path: impl AsRef<Path>,
        runner_id: Option<&str>,
    ) -> Result<Vec<Self>, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut defaults = default_paths()?;
        defaults.config_dir = config_file_directory(path)?;
        Self::from_toml_str_for_daemon(&contents, runner_id, defaults)
            .map_err(|error| error.with_path(path))
    }

    fn from_toml_str_for_daemon(
        contents: &str,
        runner_id: Option<&str>,
        defaults: DefaultPaths,
    ) -> Result<Vec<Self>, ConfigError> {
        if let Some(runner_id) = runner_id {
            return Ok(vec![Self::from_toml_str_with_defaults_and_runner(
                contents,
                defaults,
                Some(runner_id),
            )?]);
        }

        let raw: RawConfig =
            toml::from_str(contents).map_err(|source| ConfigError::Parse { path: None, source })?;
        let runner_ids = raw
            .runners
            .keys()
            .filter(|id| id.as_str() != "default")
            .cloned()
            .collect::<Vec<_>>();
        if runner_ids.is_empty() {
            return Ok(vec![Self::from_toml_str_with_defaults_and_runner(
                contents, defaults, None,
            )?]);
        }

        runner_ids
            .iter()
            .map(|runner_id| {
                Self::from_toml_str_with_defaults_and_runner(
                    contents,
                    defaults.clone(),
                    Some(runner_id),
                )
            })
            .collect()
    }

    /// Parse TOML without a config file path. Relative daemon-side paths use
    /// the platform config directory as their base.
    pub fn from_toml_str(contents: &str) -> Result<Self, ConfigError> {
        Self::from_toml_str_for_runner(contents, None)
    }

    /// Parse configuration TOML and select one named runner definition.
    pub fn from_toml_str_for_runner(
        contents: &str,
        runner_id: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_str_with_defaults_and_runner(contents, default_paths()?, runner_id)
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

    #[cfg(test)]
    fn from_toml_str_with_defaults(
        contents: &str,
        defaults: DefaultPaths,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_str_with_defaults_and_runner(contents, defaults, None)
    }

    fn from_toml_str_with_defaults_and_runner(
        contents: &str,
        defaults: DefaultPaths,
        runner_id: Option<&str>,
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

        let raw_storage = raw.storage;
        let (local_id, runner, raw_overrides, credentials_path) = select_runner(
            raw.runner,
            raw.runners,
            raw.overrides,
            raw_storage.credentials_file.clone(),
            runner_id,
            &defaults,
        )?;
        let scope = local_id
            .as_deref()
            .map(|id| format!("[runners.{id}]"))
            .unwrap_or_else(|| "[runner]".to_owned());

        let runner_name = runner
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ConfigError::Invalid(format!("missing {scope}.name")))?;
        let legacy_workspace_parent = expand_path(
            runner
                .workspace_parent
                .as_deref()
                .unwrap_or(&defaults.workspace_parent),
            &defaults.home,
        )?;
        let workspace_parent = runner.workspace_parent.clone();
        let mut legacy_workspace_roots = vec![legacy_workspace_parent];
        for workspace_parent in raw_overrides
            .iter()
            .filter_map(|rule| rule.workspace_parent.as_deref())
        {
            let workspace_parent = expand_path(workspace_parent, &defaults.home)?;
            if !legacy_workspace_roots.contains(&workspace_parent) {
                legacy_workspace_roots.push(workspace_parent);
            }
        }
        let executor_cwd = resolve_executor_cwd(
            runner.executor_cwd.as_deref().ok_or_else(|| {
                ConfigError::Invalid(
                    format!("missing required {scope}.executor_cwd; set the daemon-side working directory for the executor transport")
                        .to_owned(),
                )
            })?,
            &defaults.home,
            &defaults.config_dir,
        )?;
        let credentials_file =
            resolve_daemon_path(&credentials_path, &defaults.home, &defaults.config_dir)?;
        let state_dir = resolve_state_dir(
            raw_storage
                .state_dir
                .as_deref()
                .unwrap_or(&defaults.state_dir),
            &defaults.home,
            &defaults.config_dir,
        )?;
        let active_runs_file =
            active_runs_file(&state_dir, local_id.as_deref().unwrap_or("default"));
        let legacy_active_runs_file = if local_id.is_some() {
            legacy_named_active_runs_file(&credentials_file)
        } else {
            credentials_file.with_file_name("active-runs.json")
        };

        let max_concurrent = runner.max_concurrent.unwrap_or(1);
        if max_concurrent == 0 {
            return Err(ConfigError::Invalid(format!(
                "{scope}.max_concurrent must be greater than zero"
            )));
        }
        let poll_interval_seconds = runner.poll_interval_seconds.unwrap_or(15);
        if poll_interval_seconds == 0 {
            return Err(ConfigError::Invalid(format!(
                "{scope}.poll_interval_seconds must be greater than zero"
            )));
        }

        let keep_workspaces_for_hours = raw_storage.keep_workspaces_for_hours.unwrap_or(72);
        let keep_workspaces_max = raw_storage.keep_workspaces_max.unwrap_or(20);
        let keep_workspaces_max_age_seconds = keep_workspaces_for_hours
            .checked_mul(60 * 60)
            .ok_or_else(|| {
                ConfigError::Invalid("[storage].keep_workspaces_for_hours is too large".to_owned())
            })?;
        let overrides = raw_overrides
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
                        "each runner override must set at least one override value".to_owned(),
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
                        .map(|path| {
                            resolve_executor_cwd(path, &defaults.home, &defaults.config_dir)
                        })
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;

        Ok(Self {
            local_id: local_id.unwrap_or_else(|| "default".to_owned()),
            server_url,
            runner_name,
            runner_type: runner.runner_type.unwrap_or(RunnerType::Codex),
            state_dir,
            workspace_parent,
            legacy_workspace_roots,
            executor: runner
                .executor
                .unwrap_or_else(|| vec!["tines-runner-rs".to_owned()]),
            capabilities_executor: runner.capabilities_executor,
            custom_command: runner
                .custom_command
                .map(|command| validate_custom_command(&command).map(|()| command))
                .transpose()?,
            repository_checkout: runner.repository_checkout.unwrap_or_default(),
            run_key_delivery: runner.run_key_delivery.unwrap_or_default(),
            executor_cwd,
            max_concurrent,
            allow_remote_concurrency: runner.allow_remote_concurrency.unwrap_or(false),
            poll_interval: Duration::from_secs(poll_interval_seconds),
            credentials_file,
            active_runs_file,
            legacy_active_runs_file,
            workspace_retention: WorkspaceRetention {
                mode: raw_storage.keep_workspaces.unwrap_or_default(),
                max_age: Duration::from_secs(keep_workspaces_max_age_seconds),
                max_count: keep_workspaces_max,
            },
            overrides,
        })
    }
}

fn select_runner(
    legacy_runner: Option<RawRunner>,
    mut runners: BTreeMap<String, RawRunner>,
    legacy_overrides: Vec<RawOverride>,
    legacy_credentials_file: Option<PathBuf>,
    requested_id: Option<&str>,
    defaults: &DefaultPaths,
) -> Result<(Option<String>, RawRunner, Vec<RawOverride>, PathBuf), ConfigError> {
    if requested_id == Some("default") && runners.contains_key("default") {
        return Err(ConfigError::Invalid(
            "runner ID \"default\" is reserved for [runners.default], which is a template and cannot be selected".to_owned(),
        ));
    }

    let runner_template = runners.remove("default");
    if !runners.is_empty() || runner_template.is_some() {
        if legacy_runner.is_some() {
            return Err(ConfigError::Invalid(
                "use either [runner] or [runners.<id>] definitions, not both".to_owned(),
            ));
        }
        if !legacy_overrides.is_empty() {
            return Err(ConfigError::Invalid(
                "put each assignment override under its runner as [[runners.<id>.override]]"
                    .to_owned(),
            ));
        }
        if legacy_credentials_file.is_some() {
            return Err(ConfigError::Invalid(
                "set credentials_file inside each [runners.<id>] definition".to_owned(),
            ));
        }

        if let Some(template) = &runner_template {
            validate_runner_template(template)?;
            for (id, runner) in &mut runners {
                if runner
                    .name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty())
                {
                    return Err(ConfigError::Invalid(format!(
                        "missing required [runners.{id}].name; name must be set on each named runner and cannot be inherited from [runners.default]"
                    )));
                }
                if runner.runner_type.is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "missing required [runners.{id}].runner_type; runner_type must be set on each named runner and cannot be inherited from [runners.default]"
                    )));
                }
                runner.inherit_settings_from(template);
            }
        }

        if runners.is_empty() {
            return Err(ConfigError::Invalid(
                "missing named [runners.<id>] definition; [runners.default] is a template and cannot be selected".to_owned(),
            ));
        }

        let names = runners.keys().cloned().collect::<Vec<_>>();
        for id in &names {
            if id.trim().is_empty() || id.trim() != id || id.chars().any(char::is_control) {
                return Err(ConfigError::Invalid(format!(
                    "invalid runner id {id:?}; IDs must be non-empty and contain no surrounding or control characters"
                )));
            }
        }
        let selected_id = match requested_id {
            Some(id) if runners.contains_key(id) => id.to_owned(),
            Some(id) => {
                return Err(ConfigError::Invalid(format!(
                    "unknown runner id {id:?}; configured runner IDs: {}",
                    names.join(", ")
                )));
            }
            None if runners.len() == 1 => names[0].clone(),
            None => {
                return Err(ConfigError::Invalid(format!(
                    "--runner <id> is required because this config defines multiple runners: {}",
                    names.join(", ")
                )));
            }
        };

        let mut credential_owners = BTreeMap::<PathBuf, String>::new();
        for (id, runner) in &runners {
            let credentials_file = runner.credentials_file.as_deref().ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "missing required [runners.{id}].credentials_file; each runner must use its own credentials file"
                ))
            })?;
            if credentials_file.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[runners.{id}].credentials_file must not be empty"
                )));
            }
            let credentials_file =
                resolve_daemon_path(credentials_file, &defaults.home, &defaults.config_dir)?;
            let comparison_path = normalize_path_for_comparison(&credentials_file);
            if let Some(other_id) = credential_owners.insert(comparison_path, id.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "[runners.{other_id}].credentials_file and [runners.{id}].credentials_file resolve to the same path; each runner must use a distinct credentials file"
                )));
            }
        }

        let mut runner = runners
            .remove(&selected_id)
            .expect("selected runner was verified above");
        let credentials_file = runner
            .credentials_file
            .clone()
            .expect("all named runners were required to configure credentials_file");
        let overrides = std::mem::take(&mut runner.overrides);
        return Ok((Some(selected_id), runner, overrides, credentials_file));
    }

    if requested_id.is_some() {
        return Err(ConfigError::Invalid(
            "--runner can only be used with named [runners.<id>] definitions".to_owned(),
        ));
    }
    let runner = legacy_runner.ok_or_else(|| {
        ConfigError::Invalid("missing [runner] or [runners.<id>] definition".to_owned())
    })?;
    if !runner.overrides.is_empty() {
        return Err(ConfigError::Invalid(
            "assignment overrides must use top-level [[override]] with [runner]".to_owned(),
        ));
    }
    if runner.credentials_file.is_some() && legacy_credentials_file.is_some() {
        return Err(ConfigError::Invalid(
            "set credentials_file in either [runner] or [storage], not both".to_owned(),
        ));
    }
    let credentials_file = runner
        .credentials_file
        .clone()
        .or(legacy_credentials_file)
        .unwrap_or_else(|| defaults.credentials_file.clone());
    Ok((None, runner, legacy_overrides, credentials_file))
}

fn validate_runner_template(template: &RawRunner) -> Result<(), ConfigError> {
    if template.name.is_some()
        || template.runner_type.is_some()
        || template.credentials_file.is_some()
    {
        return Err(ConfigError::Invalid(
            "[runners.default] is a template and cannot define name, runner_type, or credentials_file; set these fields on each named runner".to_owned(),
        ));
    }
    if !template.overrides.is_empty() {
        return Err(ConfigError::Invalid(
            "[runners.default] cannot define assignment overrides; keep [[runners.<id>.override]] entries on each named runner".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_path_for_comparison(path: &Path) -> PathBuf {
    normalize_path_lexically(path)
}

fn active_runs_file(state_dir: &Path, runner_id: &str) -> PathBuf {
    state_dir
        .join(format!("runner-{}", runner_state_namespace(runner_id)))
        .join("active-runs.json")
}

fn runner_state_namespace(runner_id: &str) -> String {
    let digest = Sha256::digest(runner_id.as_bytes());
    let mut namespace = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        namespace.push(char::from(HEX[usize::from(byte >> 4)]));
        namespace.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    namespace
}

fn legacy_named_active_runs_file(credentials_file: &Path) -> PathBuf {
    let normalized_credentials = normalize_path_for_comparison(credentials_file);
    let digest = Sha256::digest(normalized_credentials.to_string_lossy().as_bytes());
    let suffix = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let parent = credentials_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("active-runs-{suffix}.json"))
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
    runner: Option<RawRunner>,
    #[serde(default)]
    runners: BTreeMap<String, RawRunner>,
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
    credentials_file: Option<PathBuf>,
    runner_type: Option<RunnerType>,
    workspace_parent: Option<PathBuf>,
    executor: Option<Vec<String>>,
    capabilities_executor: Option<Vec<String>>,
    custom_command: Option<Vec<String>>,
    repository_checkout: Option<RepositoryCheckoutPolicy>,
    run_key_delivery: Option<RunKeyDelivery>,
    executor_cwd: Option<PathBuf>,
    max_concurrent: Option<usize>,
    allow_remote_concurrency: Option<bool>,
    poll_interval_seconds: Option<u64>,
    #[serde(default, rename = "override")]
    overrides: Vec<RawOverride>,
}

impl RawRunner {
    /// Copy settings from the shared template when this runner does not set
    /// them. Lists are replaced as values and assignment overrides stay local.
    fn inherit_settings_from(&mut self, template: &Self) {
        if self.workspace_parent.is_none() {
            self.workspace_parent.clone_from(&template.workspace_parent);
        }
        if self.executor.is_none() {
            self.executor.clone_from(&template.executor);
        }
        if self.capabilities_executor.is_none() {
            self.capabilities_executor
                .clone_from(&template.capabilities_executor);
        }
        if self.custom_command.is_none() {
            self.custom_command.clone_from(&template.custom_command);
        }
        if self.repository_checkout.is_none() {
            self.repository_checkout
                .clone_from(&template.repository_checkout);
        }
        if self.run_key_delivery.is_none() {
            self.run_key_delivery.clone_from(&template.run_key_delivery);
        }
        if self.executor_cwd.is_none() {
            self.executor_cwd.clone_from(&template.executor_cwd);
        }
        if self.max_concurrent.is_none() {
            self.max_concurrent.clone_from(&template.max_concurrent);
        }
        if self.allow_remote_concurrency.is_none() {
            self.allow_remote_concurrency
                .clone_from(&template.allow_remote_concurrency);
        }
        if self.poll_interval_seconds.is_none() {
            self.poll_interval_seconds
                .clone_from(&template.poll_interval_seconds);
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    credentials_file: Option<PathBuf>,
    state_dir: Option<PathBuf>,
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
    /// Base directory for daemon-side paths from the selected config file.
    config_dir: PathBuf,
    workspace_parent: PathBuf,
    credentials_file: PathBuf,
    state_dir: PathBuf,
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
        env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        env::var_os("APPDATA").map(PathBuf::from),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
    ))
}

pub(crate) fn default_credentials_path() -> Result<PathBuf, ConfigError> {
    Ok(default_paths()?.credentials_file)
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
    xdg_state_home: Option<PathBuf>,
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

    #[cfg(windows)]
    let state_dir = absolute_path(xdg_state_home)
        .or_else(|| absolute_path(_local_appdata))
        .unwrap_or_else(|| home.join("AppData").join("Local"))
        .join("tines-runner-rs");
    #[cfg(target_os = "macos")]
    let state_dir = absolute_path(xdg_state_home)
        .unwrap_or_else(|| home.join("Library").join("Application Support"))
        .join("tines-runner-rs");
    #[cfg(all(unix, not(target_os = "macos")))]
    let state_dir = absolute_path(xdg_state_home)
        .unwrap_or_else(|| home.join(".local").join("state"))
        .join("tines-runner-rs");

    DefaultPaths {
        home: home.clone(),
        config_dir: config_dir.clone(),
        credentials_file: config_dir.join("credentials.toml"),
        workspace_parent: data_dir.join("tines-runner-rs").join("workspaces"),
        state_dir,
    }
}

fn resolve_executor_cwd(
    path: &Path,
    home: &Path,
    config_dir: &Path,
) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid(
            "executor_cwd must not be empty".to_owned(),
        ));
    }
    resolve_daemon_path(path, home, config_dir)
}

fn resolve_state_dir(path: &Path, home: &Path, config_dir: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid(
            "state_dir must not be empty".to_owned(),
        ));
    }
    resolve_daemon_path(path, home, config_dir)
}

fn resolve_daemon_path(
    path: &Path,
    home: &Path,
    config_dir: &Path,
) -> Result<PathBuf, ConfigError> {
    let expanded = expand_path(path, home)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(config_dir.join(expanded))
    }
}

fn config_file_directory(path: &Path) -> Result<PathBuf, ConfigError> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| {
                ConfigError::Invalid(format!("could not resolve config file directory: {error}"))
            })?
            .join(path)
    };
    let directory = absolute_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "could not determine parent directory for config file {}",
                path.display()
            ))
        })?;
    Ok(directory)
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let is_absolute = path.is_absolute();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some_and(|name| name != "..") {
                    normalized.pop();
                } else if !is_absolute {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
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

    #[test]
    fn antigravity_pi_alias_stays_at_the_protocol_boundary() {
        let runner = RunnerType::Antigravity;
        assert_eq!(runner.executor_harness(), "antigravity");
        assert_eq!(runner.tines_harness(), crate::protocol::RunnerHarness::Pi);
        assert_eq!(runner.effort_capability_harness(), Some("antigravity"));
        assert_eq!(runner.tines_capability_harness(), Some("pi"));
        assert!(!runner.supports_continuation());
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("tines-runner-config-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create config test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn defaults() -> DefaultPaths {
        let home = PathBuf::from("/home/tester");
        let config_dir = home.join(".config/tines-runner-rs");
        DefaultPaths {
            home: home.clone(),
            config_dir: config_dir.clone(),
            workspace_parent: home.join(".local/share/tines-runner-rs/workspaces"),
            credentials_file: config_dir.join("credentials.toml"),
            state_dir: home.join(".local/state/tines-runner-rs"),
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
    fn named_runners_are_selected_independently_from_one_config() {
        let contents = r#"
[server]
url = "https://tines.example.test"

[runners.codex]
name = "workstation-codex"
credentials_file = "/var/lib/tines/codex-credentials.toml"
runner_type = "codex"
executor = ["codex-executor"]
capabilities_executor = ["codex-probe"]
executor_cwd = "/srv/codex"
max_concurrent = 2

[[runners.codex.override]]
project = "Payments"
executor = ["payments-executor"]

[runners.antigravity]
name = "workstation-antigravity"
credentials_file = "/var/lib/tines/antigravity-credentials.toml"
runner_type = "antigravity"
executor = ["antigravity-executor"]
capabilities_executor = ["antigravity-probe"]
executor_cwd = "/srv/antigravity"
max_concurrent = 4

[[runners.antigravity.override]]
state = "Review"
executor = ["antigravity-review-executor"]

[storage]
state_dir = "/var/lib/tines/state"
"#;

        let codex =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("codex"))
                .unwrap();
        assert_eq!(codex.local_id, "codex");
        assert_eq!(codex.runner_name, "workstation-codex");
        assert_eq!(codex.runner_type, RunnerType::Codex);
        assert_eq!(
            codex.credentials_file,
            PathBuf::from("/var/lib/tines/codex-credentials.toml")
        );
        assert_eq!(codex.state_dir, PathBuf::from("/var/lib/tines/state"));
        assert_eq!(
            codex.active_runs_file(),
            active_runs_file(Path::new("/var/lib/tines/state"), "codex")
        );
        assert_eq!(codex.executor, ["codex-executor".to_owned()]);
        assert_eq!(
            codex.capabilities_executor,
            Some(vec!["codex-probe".to_owned()])
        );
        assert_eq!(codex.executor_cwd, PathBuf::from("/srv/codex"));
        assert_eq!(codex.max_concurrent, 2);
        assert_eq!(
            codex
                .resolve(context("Payments", "Build", "Ready"))
                .executor,
            ["payments-executor".to_owned()]
        );

        let antigravity = Config::from_toml_str_with_defaults_and_runner(
            contents,
            defaults(),
            Some("antigravity"),
        )
        .unwrap();
        assert_eq!(antigravity.local_id, "antigravity");
        assert_eq!(antigravity.runner_name, "workstation-antigravity");
        assert_eq!(antigravity.runner_type, RunnerType::Antigravity);
        assert_eq!(
            antigravity.credentials_file,
            PathBuf::from("/var/lib/tines/antigravity-credentials.toml")
        );
        assert_eq!(antigravity.state_dir, PathBuf::from("/var/lib/tines/state"));
        assert_eq!(
            antigravity.active_runs_file(),
            active_runs_file(Path::new("/var/lib/tines/state"), "antigravity")
        );
        assert_eq!(antigravity.executor, ["antigravity-executor".to_owned()]);
        assert_eq!(
            antigravity.capabilities_executor,
            Some(vec!["antigravity-probe".to_owned()])
        );
        assert_eq!(antigravity.executor_cwd, PathBuf::from("/srv/antigravity"));
        assert_eq!(antigravity.max_concurrent, 4);
        assert_eq!(
            antigravity
                .resolve(context("Payments", "Build", "Ready"))
                .executor,
            ["antigravity-executor".to_owned()]
        );
        assert_eq!(
            antigravity
                .resolve(context("Payments", "Build", "Review"))
                .executor,
            ["antigravity-review-executor".to_owned()]
        );
        assert_ne!(codex.active_runs_file(), antigravity.active_runs_file());
        assert_ne!(
            codex.active_runs_file().parent(),
            antigravity.active_runs_file().parent()
        );
    }

    #[test]
    fn named_runners_inherit_shared_settings_and_replace_lists() {
        let contents = r#"
[server]
url = "https://tines.example.test"

[runners.default]
executor = ["shared-transport", "--mounted"]
capabilities_executor = ["shared-probe", "--mounted"]
executor_cwd = "~/daemon"
workspace_parent = "~/workspaces"
max_concurrent = 5
repository_checkout = "metadata_only"
run_key_delivery = "environment"
poll_interval_seconds = 9
allow_remote_concurrency = true

[runners.codex]
name = "shared-codex"
credentials_file = "/var/lib/tines/codex.toml"
runner_type = "codex"
capabilities_executor = ["codex-probe"]
max_concurrent = 2

[runners.checks]
name = "shared-checks"
credentials_file = "/var/lib/tines/checks.toml"
runner_type = "custom"
custom_command = ["run-checks", "{prompt_file}"]
run_key_delivery = "request"

[runners.customized]
name = "customized"
credentials_file = "/var/lib/tines/customized.toml"
runner_type = "codex"
executor = ["custom-transport", "--runner-only"]

[[runners.checks.override]]
project = "Payments"
repository_checkout = "enabled"
"#;

        let codex =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("codex"))
                .unwrap();
        assert_eq!(codex.executor, ["shared-transport", "--mounted"]);
        assert_eq!(
            codex.capabilities_executor,
            Some(vec!["codex-probe".to_owned()])
        );
        assert_eq!(codex.executor_cwd, PathBuf::from("/home/tester/daemon"));
        assert_eq!(codex.workspace_parent, Some(PathBuf::from("~/workspaces")));
        assert_eq!(codex.max_concurrent, 2);
        assert_eq!(
            codex.repository_checkout,
            RepositoryCheckoutPolicy::MetadataOnly
        );
        assert_eq!(codex.run_key_delivery, RunKeyDelivery::Environment);
        assert_eq!(codex.poll_interval, Duration::from_secs(9));
        assert!(codex.allow_remote_concurrency);

        let checks =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("checks"))
                .unwrap();
        assert_eq!(checks.executor, codex.executor);
        assert_eq!(
            checks.capabilities_executor,
            Some(vec!["shared-probe".to_owned(), "--mounted".to_owned()])
        );
        assert_eq!(checks.executor_cwd, codex.executor_cwd);
        assert_eq!(checks.workspace_parent, codex.workspace_parent);
        assert_eq!(checks.max_concurrent, 5);
        assert_eq!(
            checks.repository_checkout,
            RepositoryCheckoutPolicy::MetadataOnly
        );
        assert_eq!(checks.run_key_delivery, RunKeyDelivery::Request);
        assert_eq!(checks.poll_interval, Duration::from_secs(9));
        assert!(checks.allow_remote_concurrency);
        assert_eq!(
            checks.custom_command,
            Some(vec!["run-checks".to_owned(), "{prompt_file}".to_owned()])
        );
        assert_eq!(
            checks
                .resolve(context("Payments", "Build", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::Enabled
        );
        assert_eq!(
            checks
                .resolve(context("Other", "Build", "Ready"))
                .repository_checkout,
            RepositoryCheckoutPolicy::MetadataOnly
        );

        let customized = Config::from_toml_str_with_defaults_and_runner(
            contents,
            defaults(),
            Some("customized"),
        )
        .unwrap();
        assert_eq!(customized.executor, ["custom-transport", "--runner-only"]);
        assert_ne!(customized.executor, codex.executor);
    }

    #[test]
    fn runner_template_does_not_supply_identity_fields() {
        let template_identity = r#"
[server]
url = "https://tines.example.test"
[runners.default]
name = "must-not-register"
executor_cwd = "/daemon"
[runners.worker]
name = "worker"
credentials_file = "/var/lib/tines/worker.toml"
runner_type = "codex"
"#;
        let error = Config::from_toml_str_with_defaults_and_runner(
            template_identity,
            defaults(),
            Some("worker"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("[runners.default] is a template"), "{error}");
        assert!(error.contains("cannot define name, runner_type, or credentials_file"));

        for (runner_fields, required_field) in [
            (
                "credentials_file = \"/var/lib/tines/worker.toml\"\nrunner_type = \"codex\"\n",
                ".name",
            ),
            (
                "name = \"worker\"\ncredentials_file = \"/var/lib/tines/worker.toml\"\n",
                ".runner_type",
            ),
            (
                "name = \"worker\"\nrunner_type = \"codex\"\n",
                ".credentials_file",
            ),
        ] {
            let contents = format!(
                "[server]\nurl = \"https://tines.example.test\"\n[runners.default]\nexecutor_cwd = \"/daemon\"\n[runners.worker]\n{runner_fields}"
            );
            let error = Config::from_toml_str_with_defaults_and_runner(
                &contents,
                defaults(),
                Some("worker"),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(&format!("[runners.worker]{required_field}")),
                "expected required field {required_field}, got: {error}"
            );
        }

        let missing_effective = r#"
[server]
url = "https://tines.example.test"
[runners.default]
max_concurrent = 3
[runners.worker]
name = "worker"
credentials_file = "/var/lib/tines/worker.toml"
runner_type = "codex"
"#;
        let error = Config::from_toml_str_with_defaults_and_runner(
            missing_effective,
            defaults(),
            Some("worker"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("missing required [runners.worker].executor_cwd"),
            "{error}"
        );
    }

    #[test]
    fn custom_harness_command_inherits_and_named_command_replaces_it() {
        let contents = r#"
[server]
url = "https://tines.example.test"
[runners.default]
executor_cwd = "/daemon"
custom_command = ["shared-checks", "--prompt", "{prompt_file}"]
[runners.inherited]
name = "inherited"
credentials_file = "/var/lib/tines/inherited.toml"
runner_type = "custom"
[runners.replaced]
name = "replaced"
credentials_file = "/var/lib/tines/replaced.toml"
runner_type = "custom"
custom_command = ["runner-checks", "{workspace}"]
"#;

        let inherited =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("inherited"))
                .unwrap();
        assert_eq!(
            inherited.custom_command,
            Some(vec![
                "shared-checks".to_owned(),
                "--prompt".to_owned(),
                "{prompt_file}".to_owned()
            ])
        );

        let replaced =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("replaced"))
                .unwrap();
        assert_eq!(
            replaced.custom_command,
            Some(vec!["runner-checks".to_owned(), "{workspace}".to_owned()])
        );
    }

    #[test]
    fn runner_template_cannot_be_selected_or_supply_assignment_overrides() {
        let template_only = r#"
[server]
url = "https://tines.example.test"
[runners.default]
executor_cwd = "/daemon"
[runners.worker]
name = "worker"
credentials_file = "/var/lib/tines/worker.toml"
runner_type = "codex"
"#;
        let error = Config::from_toml_str_with_defaults_and_runner(
            template_only,
            defaults(),
            Some("default"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("[runners.default]"), "{error}");
        assert!(error.contains("cannot be selected"), "{error}");

        let template_override = template_only.replace(
            "executor_cwd = \"/daemon\"",
            "executor_cwd = \"/daemon\"\n\n[[runners.default.override]]\nproject = \"Payments\"\nexecutor = [\"alternate\"]",
        );
        let error = Config::from_toml_str_with_defaults_and_runner(
            &template_override,
            defaults(),
            Some("worker"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("cannot define assignment overrides"),
            "{error}"
        );
    }

    #[test]
    fn named_runner_selection_requires_a_known_id_for_multiple_definitions() {
        let contents = r#"
[server]
url = "https://tines.example.test"
[runners.codex]
name = "codex"
credentials_file = "/var/lib/tines/codex.toml"
executor_cwd = "/srv/codex"
[runners.antigravity]
name = "antigravity"
credentials_file = "/var/lib/tines/antigravity.toml"
executor_cwd = "/srv/antigravity"
"#;

        let omitted = Config::from_toml_str_with_defaults_and_runner(contents, defaults(), None)
            .unwrap_err()
            .to_string();
        assert!(omitted.contains("--runner <id> is required"), "{omitted}");
        let unknown =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("other"))
                .unwrap_err()
                .to_string();
        assert!(unknown.contains("unknown runner id \"other\""), "{unknown}");
        assert!(unknown.contains("antigravity, codex"), "{unknown}");
    }

    #[test]
    fn external_credentials_use_the_configured_state_directory() {
        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                credentials_file = "/run/credentials/service/runner-credentials"
                executor_cwd = "~/daemon"
                [storage]
                state_dir = "/var/lib/tines-runner-rs/state"
            "#,
            defaults(),
        )
        .unwrap();

        assert_eq!(
            config.credentials_file,
            PathBuf::from("/run/credentials/service/runner-credentials")
        );
        assert_eq!(
            config.state_dir,
            PathBuf::from("/var/lib/tines-runner-rs/state")
        );
        assert_eq!(
            config.active_runs_file(),
            active_runs_file(Path::new("/var/lib/tines-runner-rs/state"), "default")
        );
    }

    #[test]
    fn empty_state_directory_is_rejected() {
        let error = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                executor_cwd = "~/daemon"
                [storage]
                state_dir = ""
            "#,
            defaults(),
        )
        .expect_err("empty state directory must not select a hidden fallback");
        assert!(error.to_string().contains("state_dir must not be empty"));
    }

    #[test]
    fn state_directory_defaults_and_relative_paths_are_resolved_independently() {
        let default = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                credentials_file = "/run/credentials/service/runner-credentials"
                executor_cwd = "~/daemon"
            "#,
            defaults(),
        )
        .unwrap();
        assert_eq!(
            default.state_dir,
            PathBuf::from("/home/tester/.local/state/tines-runner-rs")
        );
        assert_eq!(
            default.active_runs_file().parent(),
            Some(
                active_runs_file(&default.state_dir, "default")
                    .parent()
                    .unwrap()
            )
        );

        let relative = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                executor_cwd = "~/daemon"
                [storage]
                state_dir = "runner-state"
            "#,
            defaults(),
        )
        .unwrap();
        assert_eq!(
            relative.state_dir,
            defaults().config_dir.join("runner-state")
        );
    }

    #[test]
    fn runner_state_namespaces_are_collision_safe_path_components() {
        let state_dir = Path::new("/var/lib/tines-runner");
        let codex = active_runs_file(state_dir, "codex");
        let codex_uppercase = active_runs_file(state_dir, "Codex");
        let checks = active_runs_file(state_dir, "checks");
        let separator_id = active_runs_file(state_dir, "../codex");
        let encoded_id = active_runs_file(state_dir, "..%2Fcodex");

        assert!(codex.starts_with(state_dir));
        assert!(checks.starts_with(state_dir));
        assert!(separator_id.starts_with(state_dir));
        assert_eq!(
            codex.file_name().and_then(|name| name.to_str()),
            Some("active-runs.json")
        );
        let namespace = codex.parent().and_then(Path::file_name).unwrap();
        let namespace = namespace.to_str().unwrap();
        assert!(namespace.starts_with("runner-"));
        assert_eq!(namespace.len(), "runner-".len() + 64);
        assert!(
            namespace
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert_ne!(codex, codex_uppercase);
        let folded_namespace = |path: &Path| {
            path.parent()
                .and_then(Path::file_name)
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase()
        };
        assert_ne!(folded_namespace(&codex), folded_namespace(&codex_uppercase));
        assert_ne!(separator_id, encoded_id);
    }

    #[test]
    fn named_runners_cannot_share_credentials_files() {
        let contents = r#"
[server]
url = "https://tines.example.test"
[runners.codex]
name = "codex"
credentials_file = "/var/lib/tines/credentials.toml"
executor_cwd = "/srv/codex"
[runners.antigravity]
name = "antigravity"
credentials_file = "/var/lib/tines/./credentials.toml"
executor_cwd = "/srv/antigravity"
"#;

        let error =
            Config::from_toml_str_with_defaults_and_runner(contents, defaults(), Some("codex"))
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("must use a distinct credentials file"),
            "{error}"
        );
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
        assert_eq!(state.executor_cwd, defaults().config_dir.join("review"));

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
    fn tilde_executor_cwd_resolves_under_home_and_empty_is_rejected() {
        let config = Config::from_toml_str_with_defaults(
            r#"[server]
url = "https://tines.example.test"
[runner]
name = "test-runner"
executor_cwd = "~/executor"
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
    fn file_loaded_paths_use_the_config_directory_and_keep_executor_values_opaque() {
        let directory = TestDirectory::new();
        let config_dir = directory.0.join("config");
        fs::create_dir_all(&config_dir).expect("create config directory");
        let config_path = config_dir.join("runner.toml");
        fs::write(
            &config_path,
            r#"
[server]
url = "https://tines.example.test"

[runners.default]
executor = ["transport", "file:./secrets/foo"]
capabilities_executor = ["probe", "./probe-config"]
executor_cwd = "."
workspace_parent = "./workspaces"

[runners.codex]
name = "codex"
runner_type = "codex"
credentials_file = "./credentials/codex.toml"
custom_command = ["./custom-agent", "--config", "./agent/config.toml"]

[[runners.codex.override]]
project = "Payments"
executor_cwd = "./project-cwd"
workspace_parent = "./payment-workspaces"
capabilities_executor = ["probe", "file:./payment-secrets/probe"]

[[runners.codex.override]]
workflow = "Build"
executor_cwd = "./workflow-cwd"

[[runners.codex.override]]
state = "Review"
executor_cwd = "./state-cwd"

[storage]
state_dir = "./state"
"#,
        )
        .expect("write config file");

        let configs = Config::load_for_daemon(&config_path, None).unwrap();
        assert_eq!(configs.len(), 1);
        let config = &configs[0];
        assert_ne!(std::env::current_dir().unwrap(), config_dir);
        assert_eq!(
            config.credentials_file,
            config_dir.join("credentials/codex.toml")
        );
        assert_eq!(config.state_dir, config_dir.join("state"));
        assert_eq!(config.executor_cwd, config_dir);
        assert_eq!(config.workspace_parent, Some(PathBuf::from("./workspaces")));
        assert_eq!(config.executor, ["transport", "file:./secrets/foo"]);
        assert_eq!(
            config.capabilities_executor,
            Some(vec!["probe".to_owned(), "./probe-config".to_owned()])
        );
        assert_eq!(
            config.custom_command,
            Some(vec![
                "./custom-agent".to_owned(),
                "--config".to_owned(),
                "./agent/config.toml".to_owned()
            ])
        );

        let payment = config.resolve(context("Payments", "Other", "Other"));
        assert_eq!(
            payment.executor_cwd,
            config_path.parent().unwrap().join("project-cwd")
        );
        assert_eq!(
            payment.workspace_parent,
            Some(PathBuf::from("./payment-workspaces"))
        );
        assert_eq!(
            payment.capabilities_executor,
            Some(vec![
                "probe".to_owned(),
                "file:./payment-secrets/probe".to_owned()
            ])
        );
        let workflow = config.resolve(context("Other", "Build", "Implement"));
        assert_eq!(
            workflow.executor_cwd,
            config_path.parent().unwrap().join("workflow-cwd")
        );
        let state = config.resolve(context("Other", "Other", "Review"));
        assert_eq!(
            state.executor_cwd,
            config_path.parent().unwrap().join("state-cwd")
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_paths_preserve_symlink_semantics_for_parent_components() {
        let directory = TestDirectory::new();
        let config_dir = directory.0.join("config");
        let target_dir = directory.0.join("target");
        fs::create_dir_all(&config_dir).expect("create config directory");
        fs::create_dir_all(target_dir.join("nested")).expect("create symlink target");
        fs::create_dir_all(target_dir.join("state")).expect("create target state directory");
        fs::create_dir_all(target_dir.join("executor")).expect("create target executor directory");
        fs::write(target_dir.join("credentials.toml"), "credentials")
            .expect("create target credentials file");
        std::os::unix::fs::symlink(target_dir.join("nested"), config_dir.join("link"))
            .expect("create config symlink");

        let config_path = config_dir.join("runner.toml");
        fs::write(
            &config_path,
            r#"
[server]
url = "https://tines.example.test"
[runner]
name = "symlink-path-test"
credentials_file = "link/../credentials.toml"
executor_cwd = "link/../executor"
[storage]
state_dir = "link/../state"
"#,
        )
        .expect("write config file");

        let config = Config::load(&config_path).expect("load symlink-relative paths");
        assert_eq!(
            config.credentials_file.canonicalize().unwrap(),
            target_dir.join("credentials.toml").canonicalize().unwrap()
        );
        assert_eq!(
            config.state_dir.canonicalize().unwrap(),
            target_dir.join("state").canonicalize().unwrap()
        );
        assert_eq!(
            config.executor_cwd.canonicalize().unwrap(),
            target_dir.join("executor").canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_file_parent_preserves_symlink_semantics_for_parent_components() {
        let directory = TestDirectory::new();
        let path_base = directory.0.join("path-base");
        let target_dir = directory.0.join("target");
        let actual_config_dir = target_dir.join("config");
        fs::create_dir_all(&path_base).expect("create config path base");
        fs::create_dir_all(target_dir.join("nested")).expect("create symlink target");
        fs::create_dir_all(actual_config_dir.join("credentials"))
            .expect("create target credentials directory");
        fs::create_dir_all(actual_config_dir.join("state")).expect("create target state directory");
        fs::write(
            actual_config_dir.join("credentials/codex.toml"),
            "credentials",
        )
        .expect("create target credentials file");
        std::os::unix::fs::symlink(target_dir.join("nested"), path_base.join("link"))
            .expect("create config path symlink");

        let config_path = path_base.join("link/../config/runner.toml");
        fs::write(
            &config_path,
            r#"
[server]
url = "https://tines.example.test"
[runner]
name = "symlink-config-path-test"
credentials_file = "./credentials/codex.toml"
executor_cwd = "."
[storage]
state_dir = "./state"
"#,
        )
        .expect("write config file through symlink path");

        let config = Config::load(&config_path).expect("load config through symlink path");
        assert_eq!(
            config.credentials_file.canonicalize().unwrap(),
            actual_config_dir
                .join("credentials/codex.toml")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            config.state_dir.canonicalize().unwrap(),
            actual_config_dir.join("state").canonicalize().unwrap()
        );
        assert_eq!(
            config.executor_cwd.canonicalize().unwrap(),
            actual_config_dir.canonicalize().unwrap()
        );
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
            Some(PathBuf::from("/custom/state")),
            None,
            None,
        );

        assert_eq!(
            defaults.credentials_file.parent().unwrap(),
            PathBuf::from("/custom/config/tines-runner-rs/credentials.toml")
                .parent()
                .unwrap()
        );
        assert_eq!(
            defaults.workspace_parent,
            PathBuf::from("/custom/data/tines-runner-rs/workspaces")
        );
        assert_eq!(
            defaults.state_dir,
            PathBuf::from("/custom/state/tines-runner-rs")
        );

        let config = Config::from_toml_str_with_defaults(
            r#"
                [server]
                url = "https://tines.example.test"
                [runner]
                name = "test-runner"
                executor_cwd = "~/daemon"
                credentials_file = "/run/credentials/service/runner-credentials"
            "#,
            defaults.clone(),
        )
        .unwrap();
        assert_eq!(config.legacy_workspace_roots(), [defaults.workspace_parent]);
        assert_eq!(
            config.credentials_file,
            PathBuf::from("/run/credentials/service/runner-credentials")
        );
        assert_eq!(config.state_dir, defaults.state_dir);
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
            None,
        );

        assert_eq!(
            defaults.credentials_file.parent().unwrap(),
            home.join(".config/tines-runner-rs")
        );
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
