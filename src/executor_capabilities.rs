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
        let mut discovery_error = None;
        let codex_daemon_version = daemon_version.to_owned();
        let codex_discovery = std::thread::Builder::new()
            .name("codex-capability-discovery".to_owned())
            .spawn(move || EffortCapabilities::discover(&codex_daemon_version));
        let antigravity = crate::executor::antigravity::discover(daemon_version);
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
        if effort.validate_for_harness("codex").is_err() {
            discovery_error = Some("Codex capabilities could not be verified".to_owned());
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
                discovery_error = Some(error);
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
        if discovery_error.is_none() {
            discovery_error = antigravity.error;
        }
        Self {
            version: EXECUTOR_CAPABILITIES_VERSION,
            harnesses,
            discovery_error,
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
            self.harnesses
                .get(identifier)
                .is_some_and(|capability| capability.version != "unknown")
        })
    }

    /// Return the existing Tines effort report for a harness.
    pub fn effort_report(&self, identifier: &str, daemon_version: &str) -> EffortCapabilities {
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
            self.discovery_error
                .as_deref()
                .unwrap_or("executor does not report this harness"),
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

    #[test]
    fn generic_executor_capability_document_supports_custom_without_effort() {
        let report = ExecutorCapabilities::parse(
            br#"{"version":1,"harnesses":{"custom":{"version":"tines-runner-rs 0.1.0"}}}"#,
        )
        .expect("valid custom harness report");

        assert!(report.supports("custom"));
        assert!(report.harnesses["custom"].effort.is_none());
    }
}
