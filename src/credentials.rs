//! Persistent runner credentials and bootstrap authentication.

use serde::{Deserialize, Serialize};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Runner identity and its long-lived token.
///
/// The token is private and is redacted from the debug representation. Use
/// [`CredentialStore`] to persist credentials in a separate TOML file.
#[derive(Clone, Eq, PartialEq)]
pub struct RunnerCredentials {
    runner_id: String,
    runner_token: String,
}

impl RunnerCredentials {
    /// Create a credential pair returned by runner registration.
    pub fn new(runner_id: impl Into<String>, runner_token: impl Into<String>) -> Self {
        Self {
            runner_id: runner_id.into(),
            runner_token: runner_token.into(),
        }
    }

    /// Return the server-assigned runner ID.
    pub fn runner_id(&self) -> &str {
        &self.runner_id
    }

    /// Return the runner token for authenticated protocol requests.
    pub fn runner_token(&self) -> &str {
        &self.runner_token
    }
}

impl fmt::Debug for RunnerCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerCredentials")
            .field("runner_id", &self.runner_id)
            .field("runner_token", &"[REDACTED]")
            .finish()
    }
}

/// User API key used only to bootstrap runner registration.
///
/// This type is intentionally separate from [`RunnerCredentials`] and cannot
/// be written by [`CredentialStore`]. Its debug representation is redacted.
pub struct BootstrapKey(String);

impl BootstrapKey {
    /// Read the bootstrap key from `TINES_API_KEY`.
    pub fn from_env() -> Result<Self, BootstrapKeyError> {
        Self::from_os_value(env::var_os("TINES_API_KEY"))
    }

    fn from_os_value(value: Option<OsString>) -> Result<Self, BootstrapKeyError> {
        let value = value.ok_or(BootstrapKeyError::Missing)?;
        let key = value
            .into_string()
            .map_err(|_| BootstrapKeyError::NotUnicode)?;
        Self::from_value(key)
    }

    pub(crate) fn from_value(value: impl Into<String>) -> Result<Self, BootstrapKeyError> {
        let key = value.into();
        if key.is_empty() {
            return Err(BootstrapKeyError::Empty);
        }
        Ok(Self(key))
    }

    /// Return the key for the registration request.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BootstrapKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BootstrapKey").field(&"[REDACTED]").finish()
    }
}

/// Failure to read a usable bootstrap key from the process environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapKeyError {
    /// `TINES_API_KEY` is not set.
    Missing,
    /// `TINES_API_KEY` is not valid Unicode.
    NotUnicode,
    /// `TINES_API_KEY` is set to an empty value.
    Empty,
}

impl fmt::Display for BootstrapKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("TINES_API_KEY is not set"),
            Self::NotUnicode => f.write_str("TINES_API_KEY must contain valid Unicode text"),
            Self::Empty => f.write_str("TINES_API_KEY is empty"),
        }
    }
}

impl Error for BootstrapKeyError {}

/// File-backed storage for runner credentials.
///
/// Supply a custom path with [`CredentialStore::at`], or use
/// [`CredentialStore::default_location`] for the platform configuration
/// directory's `tines-runner-rs/credentials.toml` file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// Use an explicit credentials-file path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the path used by this store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Build a store at the default platform configuration path.
    pub fn default_location() -> Result<Self, CredentialError> {
        Ok(Self::at(default_credentials_path()?))
    }

    /// Return the default credentials-file path.
    pub fn default_path() -> Result<PathBuf, CredentialError> {
        default_credentials_path()
    }

    /// Load and validate credentials from this store.
    ///
    /// Loading does not change file permissions, so externally managed
    /// credentials can be read from locations that do not allow metadata
    /// changes.
    pub fn load(&self) -> Result<RunnerCredentials, CredentialError> {
        self.load_with_file_system(&HostCredentialFileSystem)
    }

    fn load_with_file_system(
        &self,
        file_system: &impl CredentialFileSystem,
    ) -> Result<RunnerCredentials, CredentialError> {
        let metadata =
            file_system
                .metadata(&self.path)
                .map_err(|source| CredentialError::Open {
                    path: self.path.clone(),
                    source,
                })?;
        if !metadata.is_file() {
            return Err(CredentialError::Open {
                path: self.path.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "path is not a regular file"),
            });
        }

        let mut file = file_system
            .open(&self.path)
            .map_err(|source| CredentialError::Open {
                path: self.path.clone(),
                source,
            })?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|source| CredentialError::Read {
                path: self.path.clone(),
                source,
            })?;

        let document: CredentialDocument =
            toml::from_str(&contents).map_err(|_| CredentialError::Malformed {
                path: self.path.clone(),
            })?;

        if document.runner_id.is_empty() {
            return Err(CredentialError::InvalidField {
                path: self.path.clone(),
                field: "runner_id",
            });
        }
        if document.runner_token.is_empty() {
            return Err(CredentialError::InvalidField {
                path: self.path.clone(),
                field: "runner_token",
            });
        }

        Ok(RunnerCredentials::new(
            document.runner_id,
            document.runner_token,
        ))
    }

    /// Save credentials, creating parent directories as needed.
    ///
    /// On Unix, the file is created or repaired with mode `0600` before any
    /// credential data is written.
    pub fn save(&self, credentials: &RunnerCredentials) -> Result<(), CredentialError> {
        if credentials.runner_id.is_empty() {
            return Err(CredentialError::InvalidField {
                path: self.path.clone(),
                field: "runner_id",
            });
        }
        if credentials.runner_token.is_empty() {
            return Err(CredentialError::InvalidField {
                path: self.path.clone(),
                field: "runner_token",
            });
        }

        let document = CredentialDocument {
            runner_id: credentials.runner_id.clone(),
            runner_token: credentials.runner_token.clone(),
        };
        let contents = toml::to_string(&document).map_err(|_| CredentialError::Serialize)?;

        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| CredentialError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        #[cfg(unix)]
        secure_existing_file_permissions(&self.path)?;

        let mut options = OpenOptions::new();
        options.create(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|source| CredentialError::Open {
                path: self.path.clone(),
                source,
            })?;

        #[cfg(unix)]
        secure_file_permissions(&file, &self.path)?;

        file.set_len(0).map_err(|source| CredentialError::Write {
            path: self.path.clone(),
            source,
        })?;
        file.write_all(contents.as_bytes())
            .map_err(|source| CredentialError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| CredentialError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

trait CredentialFileSystem {
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }

    #[cfg(unix)]
    fn set_permissions(&self, path: &Path, permissions: fs::Permissions) -> io::Result<()> {
        fs::set_permissions(path, permissions)
    }
}

struct HostCredentialFileSystem;

impl CredentialFileSystem for HostCredentialFileSystem {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialDocument {
    runner_id: String,
    runner_token: String,
}

/// Credential-store failure. Error messages never include credential values.
#[derive(Debug)]
pub enum CredentialError {
    /// The platform configuration directory could not be determined.
    ConfigurationDirectoryUnavailable,
    /// The parent directory could not be created.
    CreateDirectory { path: PathBuf, source: io::Error },
    /// The credential file could not be opened.
    Open { path: PathBuf, source: io::Error },
    /// The credential file could not be read.
    Read { path: PathBuf, source: io::Error },
    /// The credential file is not valid TOML or omits a required field.
    Malformed { path: PathBuf },
    /// A required credential field is empty.
    InvalidField { path: PathBuf, field: &'static str },
    /// The credential document could not be serialized.
    Serialize,
    /// The credential file permissions could not be restricted to `0600`.
    SecurePermissions { path: PathBuf, source: io::Error },
    /// The credential file could not be written.
    Write { path: PathBuf, source: io::Error },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigurationDirectoryUnavailable => {
                f.write_str("could not determine the runner configuration directory")
            }
            Self::CreateDirectory { path, source } => {
                write!(
                    f,
                    "could not create credentials directory {}: {source}",
                    path.display()
                )
            }
            Self::Open { path, source } => {
                write!(
                    f,
                    "could not open credentials file {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    f,
                    "could not read credentials file {}: {source}",
                    path.display()
                )
            }
            Self::Malformed { path } => write!(
                f,
                "credentials file {} is malformed TOML or is missing a required field",
                path.display()
            ),
            Self::InvalidField { path, field } => {
                write!(
                    f,
                    "credentials file {} has an empty {field}",
                    path.display()
                )
            }
            Self::Serialize => f.write_str("could not serialize runner credentials"),
            Self::SecurePermissions { path, source } => write!(
                f,
                "could not restrict credentials file permissions for {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => {
                write!(
                    f,
                    "could not write credentials file {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for CredentialError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateDirectory { source, .. }
            | Self::Open { source, .. }
            | Self::Read { source, .. }
            | Self::SecurePermissions { source, .. }
            | Self::Write { source, .. } => Some(source),
            Self::ConfigurationDirectoryUnavailable
            | Self::Malformed { .. }
            | Self::InvalidField { .. }
            | Self::Serialize => None,
        }
    }
}

fn default_credentials_path() -> Result<PathBuf, CredentialError> {
    let config_path = crate::config::Config::default_path()
        .map_err(|_| CredentialError::ConfigurationDirectoryUnavailable)?;
    let config_dir = config_path
        .parent()
        .ok_or(CredentialError::ConfigurationDirectoryUnavailable)?;

    Ok(config_dir.join("credentials.toml"))
}

#[cfg(unix)]
fn secure_path_permissions(path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;

    HostCredentialFileSystem
        .set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| CredentialError::SecurePermissions {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(unix)]
fn secure_existing_file_permissions(path: &Path) -> Result<(), CredentialError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => secure_path_permissions(path),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CredentialError::Open {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn secure_file_permissions(file: &File, path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| CredentialError::SecurePermissions {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapKey, BootstrapKeyError, CredentialError, CredentialFileSystem, CredentialStore,
        RunnerCredentials,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

    fn temporary_file() -> PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tines-runner-credentials-{}-{id}.toml",
            std::process::id()
        ))
    }

    fn credentials() -> RunnerCredentials {
        RunnerCredentials::new("rnr_test", "runner-token-test-secret")
    }

    #[cfg(unix)]
    struct ReadOnlyCredentialFileSystem;

    #[cfg(unix)]
    impl CredentialFileSystem for ReadOnlyCredentialFileSystem {
        fn set_permissions(
            &self,
            _path: &std::path::Path,
            _permissions: fs::Permissions,
        ) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "externally managed credentials are read-only",
            ))
        }
    }

    #[test]
    fn saves_and_reloads_credentials() {
        let path = temporary_file();
        let store = CredentialStore::at(&path);
        let expected = credentials();

        store.save(&expected).unwrap();
        let actual = store.load().unwrap();

        assert_eq!(actual, expected);
        assert_eq!(actual.runner_id(), "rnr_test");
        assert_eq!(actual.runner_token(), "runner-token-test-secret");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn default_path_is_under_the_runner_configuration_directory() {
        let config_path = crate::config::Config::default_path().unwrap();
        let config_dir = config_path.parent().unwrap();

        assert_eq!(
            CredentialStore::default_path().unwrap(),
            config_dir.join("credentials.toml")
        );
    }

    #[cfg(unix)]
    #[test]
    fn creates_credentials_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_file();
        let store = CredentialStore::at(&path);
        store.save(&credentials()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn loads_read_only_external_credentials_without_changing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_file();
        let expected = credentials();
        fs::write(
            &path,
            format!(
                "runner_id = {:?}\nrunner_token = {:?}\n",
                expected.runner_id(),
                expected.runner_token()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        let actual = CredentialStore::at(&path).load().unwrap();

        assert_eq!(actual, expected);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn loads_credentials_when_permission_changes_are_denied() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_file();
        let expected = credentials();
        fs::write(
            &path,
            format!(
                "runner_id = {:?}\nrunner_token = {:?}\n",
                expected.runner_id(),
                expected.runner_token()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        let read_only_file_system = ReadOnlyCredentialFileSystem;
        let permission_error = read_only_file_system
            .set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_err();
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied
        );

        let actual = CredentialStore::at(&path)
            .load_with_file_system(&read_only_file_system)
            .unwrap();

        assert_eq!(actual, expected);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn saves_over_read_only_credentials_after_repairing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_file();
        let store = CredentialStore::at(&path);
        store.save(&credentials()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();

        let replacement = RunnerCredentials::new("rnr_replacement", "replacement-token-secret");
        store.save(&replacement).unwrap();

        assert_eq!(store.load().unwrap(), replacement);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_files_return_an_error_without_echoing_contents() {
        let path = temporary_file();
        let secret = "must-not-appear-in-error";
        fs::write(&path, format!("not valid TOML {secret}")).unwrap();

        let error = CredentialStore::at(&path).load().unwrap_err();

        assert!(matches!(error, CredentialError::Malformed { .. }));
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unreadable_path_returns_a_clear_open_error() {
        let blocker = temporary_file();
        let path = blocker.join("credentials.toml");
        fs::write(&blocker, "not a directory").unwrap();

        let error = CredentialStore::at(&path).load().unwrap_err();

        assert!(matches!(error, CredentialError::Open { .. }));
        assert!(
            error
                .to_string()
                .contains("could not open credentials file")
        );
        let _ = fs::remove_file(blocker);
    }

    #[test]
    fn debug_output_redacts_runner_and_bootstrap_tokens() {
        let runner_secret = "runner-token-debug-secret";
        let bootstrap_secret = "bootstrap-api-key-debug-secret";
        let credentials = RunnerCredentials::new("rnr_test", runner_secret);
        let bootstrap =
            BootstrapKey::from_os_value(Some(OsString::from(bootstrap_secret))).unwrap();

        let runner_debug = format!("{credentials:?}");
        let bootstrap_debug = format!("{bootstrap:?}");

        assert!(runner_debug.contains("[REDACTED]"));
        assert!(!runner_debug.contains(runner_secret));
        assert!(bootstrap_debug.contains("[REDACTED]"));
        assert!(!bootstrap_debug.contains(bootstrap_secret));
    }

    #[test]
    fn bootstrap_key_reports_missing_or_empty_values() {
        assert_eq!(
            BootstrapKey::from_os_value(None).unwrap_err(),
            BootstrapKeyError::Missing
        );
        assert_eq!(
            BootstrapKey::from_os_value(Some(OsString::new())).unwrap_err(),
            BootstrapKeyError::Empty
        );
    }
}
