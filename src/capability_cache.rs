//! Shared, event-driven capability reports for one runner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::executor_capabilities::ExecutorCapabilities;
use crate::executor_transport::{CapabilityTransportIdentity, ExecutorTransport};
use crate::logging::FailureReporter;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CapabilityKey {
    transport: CapabilityTransportIdentity,
    harness: String,
}

type CapabilityEntry = Arc<Mutex<Option<CapabilitySnapshot>>>;
type CapabilityEntries = Arc<Mutex<HashMap<CapabilityKey, CapabilityEntry>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySnapshot {
    pub capabilities: ExecutorCapabilities,
    pub revision: u64,
}

#[derive(Default, Clone)]
pub struct ExecutorCapabilityCache {
    entries: CapabilityEntries,
}

impl ExecutorCapabilityCache {
    /// Return a cached report or discover it once for this transport and harness.
    pub fn get_or_discover(
        &self,
        transport: &ExecutorTransport,
        harness: &str,
        failures: &FailureReporter,
    ) -> CapabilitySnapshot {
        if harness == "custom" {
            return CapabilitySnapshot {
                capabilities: ExecutorCapabilities::custom(crate::VERSION),
                revision: 1,
            };
        }

        let entry = self.entry(transport, harness);
        let mut snapshot = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(snapshot) = snapshot.as_ref() {
            return snapshot.clone();
        }

        let discovered = transport.discover_capabilities_reported_for_harness(failures, harness);
        let discovered = CapabilitySnapshot {
            capabilities: discovered,
            revision: 1,
        };
        *snapshot = Some(discovered.clone());
        discovered
    }

    /// Refresh only if no other worker has refreshed this snapshot already.
    /// Concurrent stale assignments therefore share one discovery result.
    pub fn refresh_if_unchanged(
        &self,
        transport: &ExecutorTransport,
        harness: &str,
        expected_revision: u64,
        failures: &FailureReporter,
    ) -> CapabilitySnapshot {
        if harness == "custom" {
            return self.get_or_discover(transport, harness, failures);
        }

        let entry = self.entry(transport, harness);
        let mut snapshot = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = snapshot.as_ref()
            && current.revision != expected_revision
        {
            return current.clone();
        }

        let revision = snapshot
            .as_ref()
            .map_or(1, |current| current.revision.saturating_add(1));
        let refreshed = CapabilitySnapshot {
            capabilities: transport.discover_capabilities_reported_for_harness(failures, harness),
            revision,
        };
        *snapshot = Some(refreshed.clone());
        refreshed
    }

    /// Seed a report already discovered by a caller.
    pub fn insert(
        &self,
        transport: &ExecutorTransport,
        harness: &str,
        capabilities: ExecutorCapabilities,
    ) -> CapabilitySnapshot {
        let entry = self.entry(transport, harness);
        let mut snapshot = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next_revision = snapshot
            .as_ref()
            .map_or(1, |current| current.revision.saturating_add(1));
        let inserted = CapabilitySnapshot {
            capabilities,
            revision: next_revision,
        };
        *snapshot = Some(inserted.clone());
        inserted
    }

    fn entry(&self, transport: &ExecutorTransport, harness: &str) -> CapabilityEntry {
        let key = CapabilityKey {
            transport: transport.capability_cache_identity(),
            harness: harness.to_owned(),
        };
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            entries
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutorCapabilityCache;
    use crate::executor_capabilities::ExecutorCapabilities;
    use crate::executor_transport::ExecutorTransport;
    use crate::logging::FailureReporter;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn custom_capabilities_are_synthesized_without_an_executor_probe() {
        let cache = ExecutorCapabilityCache::default();
        let transport = ExecutorTransport::new(Vec::new(), ".");
        let snapshot = cache.get_or_discover(
            &transport,
            "custom",
            &crate::logging::FailureReporter::default(),
        );
        assert!(snapshot.capabilities.supports("custom"));
    }

    #[test]
    fn conditional_refresh_uses_newer_concurrent_snapshot() {
        let cache = ExecutorCapabilityCache::default();
        let transport = ExecutorTransport::new(Vec::new(), ".");
        let first = cache.insert(
            &transport,
            "codex",
            ExecutorCapabilities::unavailable("initial"),
        );
        let newer = cache.insert(
            &transport,
            "codex",
            ExecutorCapabilities::unavailable("newer"),
        );

        let result = cache.refresh_if_unchanged(
            &transport,
            "codex",
            first.revision,
            &crate::logging::FailureReporter::default(),
        );

        assert_eq!(result, newer);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_refreshes_of_one_snapshot_launch_one_probe() {
        let directory = std::env::temp_dir().join(format!(
            "tines-runner-capability-cache-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("create capability-cache directory");
        let counter = directory.join("probe-count");
        let script = directory.join("capability-executor");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nprintf '%s\\n' '{{\"version\":1,\"harnesses\":{{\"codex\":{{\"version\":\"codex-test\"}}}}}}'\n",
                counter.display()
            ),
        )
        .expect("write fake capability executor");
        let mut permissions = fs::metadata(&script)
            .expect("stat fake capability executor")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("make fake executor executable");
        let transport =
            ExecutorTransport::new(vec![script.to_string_lossy().into_owned()], &directory);
        let cache = ExecutorCapabilityCache::default();
        let failures = FailureReporter::default();
        let initial = cache.get_or_discover(&transport, "codex", &failures);
        assert_eq!(fs::read(&counter).expect("read probe count").len(), 1);

        let barrier = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let cache = cache.clone();
                let transport = transport.clone();
                let failures = failures.clone();
                let barrier = Arc::clone(&barrier);
                let revision = initial.revision;
                thread::spawn(move || {
                    barrier.wait();
                    cache.refresh_if_unchanged(&transport, "codex", revision, &failures)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let snapshots = workers
            .into_iter()
            .map(|worker| worker.join().expect("join refresh worker"))
            .collect::<Vec<_>>();

        assert_eq!(fs::read(&counter).expect("read probe count").len(), 2);
        assert_eq!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[0].revision, initial.revision + 1);
        fs::remove_dir_all(directory).expect("remove capability-cache directory");
    }

    #[cfg(unix)]
    #[test]
    fn cache_identity_includes_harness_and_effective_transport() {
        let directory = std::env::temp_dir().join(format!(
            "tines-runner-capability-identity-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("create capability identity directory");
        let counter = directory.join("probe-count");
        let args = directory.join("probe-args");
        let script = directory.join("capability-executor");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '%s\\n' '{{\"version\":1,\"harnesses\":{{\"codex\":{{\"version\":\"codex-test\"}}}}}}'\n",
                counter.display(),
                args.display()
            ),
        )
        .expect("write fake capability executor");
        let mut permissions = fs::metadata(&script)
            .expect("stat fake capability executor")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("make fake executor executable");
        let transport =
            ExecutorTransport::new(vec![script.to_string_lossy().into_owned()], &directory);
        let override_transport = ExecutorTransport::new(
            vec![script.to_string_lossy().into_owned()],
            directory.join("override-cwd"),
        );
        fs::create_dir_all(directory.join("override-cwd")).expect("create override executor cwd");
        let cache = ExecutorCapabilityCache::default();
        let failures = FailureReporter::default();

        cache.get_or_discover(&transport, "codex", &failures);
        cache.get_or_discover(&transport, "codex", &failures);
        cache.get_or_discover(&transport, "antigravity", &failures);
        cache.get_or_discover(&override_transport, "codex", &failures);

        assert_eq!(fs::read(&counter).expect("read probe count").len(), 3);
        assert_eq!(
            fs::read_to_string(args).expect("read probe arguments"),
            "capabilities\n--harness\ncodex\ncapabilities\n--harness\nantigravity\ncapabilities\n--harness\ncodex\n"
        );
        fs::remove_dir_all(directory).expect("remove capability identity directory");
    }
}
