//! Versioned capability document returned by the executor environment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::effort::{EffortCapabilities, MAX_EFFORT_CAPABILITIES_AGE};

pub const EXECUTOR_CAPABILITIES_VERSION: u8 = 1;
const MAX_HARNESSES: usize = 32;
const MAX_IDENTIFIER_LENGTH: usize = 100;

/// Capabilities observed inside the configured executor environment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    pub version: u8,
    pub harnesses: BTreeMap<String, ExecutorHarnessCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<String>,
}

/// One semantic harness available to the executor.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecutorHarnessCapabilities {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<String>,
}

impl ExecutorCapabilities {
    /// Probe the native harnesses in the current executor environment.
    pub fn discover(daemon_version: &str) -> Self {
        Self::discover_with_programs(daemon_version, "codex", "agy")
    }

    fn discover_with_programs(
        daemon_version: &str,
        codex_program: &str,
        antigravity_program: &str,
    ) -> Self {
        let mut harnesses = BTreeMap::from([(
            "custom".to_owned(),
            ExecutorHarnessCapabilities {
                version: format!("tines-runner-rs {daemon_version}")
                    .chars()
                    .take(MAX_IDENTIFIER_LENGTH)
                    .collect(),
                effort: None,
                discovery_error: None,
            },
        )]);
        let codex_daemon_version = daemon_version.to_owned();
        let codex_program = codex_program.to_owned();
        let codex_discovery = std::thread::Builder::new()
            .name("codex-capability-discovery".to_owned())
            .spawn(move || {
                EffortCapabilities::discover_with_program(&codex_program, &codex_daemon_version)
            });
        let antigravity = crate::executor::antigravity::discover_with_program(
            antigravity_program,
            daemon_version,
        );
        let effort = match codex_discovery {
            Ok(discovery) => discovery.join().unwrap_or_else(|_| {
                EffortCapabilities::unavailable(
                    daemon_version,
                    "codex",
                    "Codex capability probe failed",
                )
            }),
            Err(_) => EffortCapabilities::unavailable(
                daemon_version,
                "codex",
                "Codex capability probe could not start",
            ),
        };
        if let Err(error) = effort.validate_for_harness("codex") {
            harnesses.insert(
                "codex".to_owned(),
                ExecutorHarnessCapabilities {
                    version: "unknown".to_owned(),
                    effort: None,
                    discovery_error: Some(error),
                },
            );
        } else {
            let version = effort.harness_version.clone();
            if let Some(error) = effort.discovery_error.clone() {
                harnesses.insert(
                    "codex".to_owned(),
                    ExecutorHarnessCapabilities {
                        version,
                        effort: None,
                        discovery_error: Some(error.clone()),
                    },
                );
            } else {
                harnesses.insert(
                    "codex".to_owned(),
                    ExecutorHarnessCapabilities {
                        version,
                        effort: Some(effort),
                        discovery_error: None,
                    },
                );
            }
        }

        harnesses.insert(
            "antigravity".to_owned(),
            ExecutorHarnessCapabilities {
                version: antigravity.version.unwrap_or_else(|| "unknown".to_owned()),
                effort: antigravity.effort,
                discovery_error: antigravity.error.clone(),
            },
        );
        Self {
            version: EXECUTOR_CAPABILITIES_VERSION,
            harnesses,
            discovery_error: None,
        }
    }

    /// Build an unsupported report after transport or document validation fails.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            version: EXECUTOR_CAPABILITIES_VERSION,
            harnesses: BTreeMap::new(),
            discovery_error: Some(reason.into().chars().take(200).collect()),
        }
    }

    /// Reject unsupported versions and malformed harness records.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != EXECUTOR_CAPABILITIES_VERSION {
            return Err("unsupported executor capability document version".to_owned());
        }
        if self.harnesses.len() > MAX_HARNESSES {
            return Err("executor capability document has too many harnesses".to_owned());
        }
        if self
            .discovery_error
            .as_ref()
            .is_some_and(|error| error.len() > 200)
        {
            return Err("executor capability discovery error is too long".to_owned());
        }

        for (identifier, harness) in &self.harnesses {
            if !valid_identifier(identifier)
                || harness.version.trim().is_empty()
                || harness.version.len() > MAX_IDENTIFIER_LENGTH
            {
                return Err("malformed executor harness capability".to_owned());
            }
            if let Some(effort) = &harness.effort {
                effort.validate_for_harness(identifier)?;
                if effort.harness_version != harness.version
                    || effort.discovery_error.is_some()
                    || harness.discovery_error.is_some()
                {
                    return Err("inconsistent executor effort capability report".to_owned());
                }
            }
            if harness
                .discovery_error
                .as_ref()
                .is_some_and(|error| error.len() > 200)
            {
                return Err("executor harness discovery error is too long".to_owned());
            }
        }
        Ok(())
    }

    /// Whether this document verifies the requested semantic harness.
    pub fn supports(&self, identifier: &str) -> bool {
        self.validate().is_ok_and(|()| {
            self.discovery_error.is_none()
                && self.harnesses.get(identifier).is_some_and(|capability| {
                    capability.version != "unknown" && capability.discovery_error.is_none()
                })
        })
    }

    /// Explain why a harness is not verified by this capability document.
    pub fn support_error(&self, identifier: &str) -> Option<String> {
        if let Some(error) = self.discovery_error.as_deref() {
            return Some(error.to_owned());
        }
        match self.harnesses.get(identifier) {
            Some(capability) => capability.discovery_error.clone().or_else(|| {
                (capability.version == "unknown")
                    .then(|| format!("{identifier} version could not be determined"))
            }),
            None => Some(format!("executor does not report the {identifier} harness")),
        }
    }

    /// Return the existing Tines effort report for a harness.
    pub fn effort_report(&self, identifier: &str, daemon_version: &str) -> EffortCapabilities {
        if let Some(error) = self.discovery_error.as_deref() {
            return EffortCapabilities::unavailable(daemon_version, identifier, error);
        }
        if let Some(capability) = self.harnesses.get(identifier) {
            if let Some(report) = capability.effort.as_ref() {
                return report.clone();
            }
            if let Some(error) = capability.discovery_error.as_deref() {
                return EffortCapabilities::unavailable(daemon_version, identifier, error);
            }
        }
        EffortCapabilities::unavailable(
            daemon_version,
            identifier,
            "executor does not report this harness",
        )
    }

    /// Decode and validate one capability document from executor stdout.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let capabilities: Self = serde_json::from_slice(bytes).map_err(|error| {
            format!(
                "executor returned malformed capability JSON at line {} column {}",
                error.line(),
                error.column()
            )
        })?;
        capabilities.validate()?;
        Ok(capabilities)
    }

    /// Whether the cached document has reached the standard ten-minute refresh interval.
    pub fn refresh_due(
        &self,
        refreshed_at: Option<std::time::Instant>,
        now: std::time::Instant,
    ) -> bool {
        refreshed_at
            .is_none_or(|at| now.saturating_duration_since(at) >= MAX_EFFORT_CAPABILITIES_AGE)
    }
}

fn valid_identifier(identifier: &str) -> bool {
    let bytes = identifier.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_IDENTIFIER_LENGTH
        && bytes[0].is_ascii_lowercase()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
        })
}

/// Discover capabilities in the executor's environment for the CLI command.
pub fn discover_for_cli(daemon_version: &str) -> Result<(), std::io::Error> {
    let capabilities = ExecutorCapabilities::discover(daemon_version);
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &capabilities).map_err(std::io::Error::other)?;
    std::io::Write::write_all(&mut stdout, b"\n")
}

#[cfg(test)]
mod tests {
    use super::ExecutorCapabilities;

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("stat fake executable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make fake executable");
    }

    #[test]
    fn generic_executor_capability_document_supports_custom_without_effort() {
        let report = ExecutorCapabilities::parse(
            br#"{"version":1,"harnesses":{"custom":{"version":"tines-runner-rs 0.1.0"}}}"#,
        )
        .expect("valid custom harness report");

        assert!(report.supports("custom"));
        assert!(report.harnesses["custom"].effort.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn native_harness_discovery_errors_are_scoped_to_each_harness() {
        use std::fs;
        let directory = std::env::temp_dir().join(format!(
            "tines-runner-executor-capabilities-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("create fake harness directory");
        let codex = directory.join("codex");
        fs::write(
            &codex,
            r##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.153.4\n'
  exit 0
fi
if [ "$1" = "app-server" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
      *'"method":"model/list"'*)
        printf '%s\n' '{"id":3,"result":{"data":[{"model":"gpt-5.6","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]}}'
        exit 0
        ;;
    esac
  done
fi
exit 2
"##,
        )
        .expect("write fake Codex executable");
        make_executable(&codex);
        let antigravity = directory.join("agy");
        fs::write(
            &antigravity,
            r##"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'agy v1.3.1\n'; exit 0; fi
if [ "$1" = "models" ]; then printf 'gemini-3.8-flash model\n'; exit 0; fi
exit 2
"##,
        )
        .expect("write fake Antigravity executable");
        make_executable(&antigravity);
        let missing = directory.join("missing-harness");

        for (codex_available, antigravity_available) in
            [(true, false), (false, true), (true, true), (false, false)]
        {
            let report = ExecutorCapabilities::discover_with_programs(
                "0.1.0",
                if codex_available {
                    codex.to_str().expect("Codex path is UTF-8")
                } else {
                    missing.to_str().expect("missing path is UTF-8")
                },
                if antigravity_available {
                    antigravity.to_str().expect("Antigravity path is UTF-8")
                } else {
                    missing.to_str().expect("missing path is UTF-8")
                },
            );

            assert_eq!(report.discovery_error, None);
            assert_eq!(report.supports("codex"), codex_available);
            assert_eq!(report.supports("antigravity"), antigravity_available);
            assert_eq!(
                report.harnesses["codex"].discovery_error.is_some(),
                !codex_available
            );
            assert_eq!(
                report.harnesses["antigravity"].discovery_error.is_some(),
                !antigravity_available
            );
        }

        fs::remove_dir_all(directory).expect("remove fake harness directory");
    }
}
