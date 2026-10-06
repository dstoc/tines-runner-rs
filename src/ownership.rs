//! Local ownership locks for server-side runner registrations.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use url::Url;

use crate::credentials::{CredentialStore, RunnerCredentials};

/// An exclusive local lock held for the lifetime of one daemon registration.
#[derive(Debug)]
pub struct DaemonOwnershipGuard {
    _file: File,
}

impl DaemonOwnershipGuard {
    /// Acquire the lock for this server-side registration.
    ///
    /// The lock identity is independent of the local runner ID and credential
    /// file path. A copied credential pair therefore resolves to the same lock.
    pub fn acquire(server_url: &Url, credentials: &RunnerCredentials) -> io::Result<Self> {
        let credentials_path = CredentialStore::default_path().map_err(io::Error::other)?;
        let lock_directory = credentials_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "default credentials path has no parent directory",
            )
        })?;
        fs::create_dir_all(lock_directory)?;

        let identity = format!("{}\0{}", server_url.as_str(), credentials.runner_id());
        let digest = Sha256::digest(identity.as_bytes());
        let suffix = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = lock_directory.join(format!("daemon-{suffix}.lock"));
        acquire_at(path)
    }
}

fn acquire_at(path: PathBuf) -> io::Result<DaemonOwnershipGuard> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(DaemonOwnershipGuard { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "another local daemon already owns this runner registration (lock: {})",
                path.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::acquire_at;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn lock_path() -> PathBuf {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "tines-runner-ownership-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create ownership lock test directory");
        directory.join("registration.lock")
    }

    #[test]
    fn lock_is_exclusive_until_guard_is_dropped_and_lock_file_can_be_reused() {
        let path = lock_path();
        let guard = acquire_at(path.clone()).expect("acquire first registration lock");
        let error = acquire_at(path.clone()).expect_err("second registration lock must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(guard);
        let replacement = acquire_at(path.clone()).expect("reacquire released registration lock");
        drop(replacement);
        fs::remove_file(&path).expect("remove test lock file");
        fs::remove_dir(path.parent().expect("lock file has a parent"))
            .expect("remove ownership lock test directory");
    }

    #[test]
    fn different_registration_lock_files_can_be_held_at_the_same_time() {
        let first = lock_path();
        let second = first.with_file_name("other-registration.lock");
        let first_guard = acquire_at(first.clone()).expect("acquire first registration lock");
        let second_guard = acquire_at(second.clone()).expect("acquire second registration lock");
        drop((first_guard, second_guard));
        fs::remove_file(&first).expect("remove first test lock file");
        fs::remove_file(&second).expect("remove second test lock file");
        fs::remove_dir(first.parent().expect("lock file has a parent"))
            .expect("remove ownership lock test directory");
    }
}
