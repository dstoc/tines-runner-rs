//! Structured diagnostics for the long-running runner process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;

/// Initialize JSON tracing output with a filter from `RUST_LOG` or `info`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
}

/// Report a concurrency policy only when its effective local value changes.
pub fn log_effective_concurrency_change(previous: u32, current: u32) {
    if previous != current {
        tracing::info!(
            previous,
            concurrency = current,
            "effective runner concurrency changed"
        );
    }
}

/// Tracks one degraded operational path so retries can be summarized instead
/// of producing the same warning on every attempt.
#[derive(Clone, Default)]
pub struct FailureReporter {
    episodes: Arc<Mutex<HashMap<String, FailureEpisode>>>,
}

struct FailureEpisode {
    signature: String,
    failures: u64,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureDisposition {
    First,
    Repeated { failures: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoverySummary {
    pub failures: u64,
    pub outage: Duration,
}

impl FailureReporter {
    /// Record a failure for a named path. A changed signature starts a new
    /// episode so a new root cause is visible immediately.
    pub fn record_failure(&self, path: &str, signature: &str) -> FailureDisposition {
        let now = Instant::now();
        let mut episodes = self
            .episodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match episodes.get_mut(path) {
            Some(episode) if episode.signature == signature => {
                episode.failures = episode.failures.saturating_add(1);
                FailureDisposition::Repeated {
                    failures: episode.failures,
                }
            }
            _ => {
                episodes.insert(
                    path.to_owned(),
                    FailureEpisode {
                        signature: signature.to_owned(),
                        failures: 1,
                        started_at: now,
                    },
                );
                FailureDisposition::First
            }
        }
    }

    /// Clear a recovered path and return a concise summary of its outage.
    pub fn recovered(&self, path: &str) -> Option<RecoverySummary> {
        let episode = self
            .episodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(path)?;
        Some(RecoverySummary {
            failures: episode.failures,
            outage: episode.started_at.elapsed(),
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Debug)]
    pub(crate) struct CapturedEvent {
        pub(crate) level: Level,
        pub(crate) fields: String,
    }

    #[derive(Clone, Default)]
    struct CaptureLayer(Arc<Mutex<Vec<CapturedEvent>>>);

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut fields = EventFields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields: fields.0,
                });
        }
    }

    #[derive(Default)]
    struct EventFields(String);

    impl Visit for EventFields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={value:?} ", field.name()));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push_str(&format!("{}={value} ", field.name()));
        }
    }

    pub(crate) fn capture_events(run: impl FnOnce()) -> Vec<CapturedEvent> {
        let layer = CaptureLayer::default();
        let events = Arc::clone(&layer.0);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, run);
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureDisposition, FailureReporter, log_effective_concurrency_change};
    use crate::logging::test_support::capture_events;

    #[test]
    fn failure_reporter_counts_retries_and_clears_on_recovery() {
        let reporter = FailureReporter::default();

        assert_eq!(
            reporter.record_failure("poll", "unavailable"),
            FailureDisposition::First
        );
        assert_eq!(
            reporter.record_failure("poll", "unavailable"),
            FailureDisposition::Repeated { failures: 2 }
        );
        assert_eq!(reporter.recovered("poll").unwrap().failures, 2);
        assert!(reporter.recovered("poll").is_none());
        assert_eq!(
            reporter.record_failure("poll", "unauthorized"),
            FailureDisposition::First
        );
    }

    #[test]
    fn concurrency_logging_is_info_only_when_the_effective_value_changes() {
        let events = capture_events(|| {
            log_effective_concurrency_change(3, 3);
            log_effective_concurrency_change(3, 2);
        });

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, tracing::Level::INFO);
        assert!(
            events[0]
                .fields
                .contains("effective runner concurrency changed")
        );
        assert!(events[0].fields.contains("concurrency=2"));
    }
}
