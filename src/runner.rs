//! Runner registration, credential reuse, and authenticated protocol access.

use std::env;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{Config, RunnerType};
use crate::credentials::{
    BootstrapKey, BootstrapKeyError, CredentialError, CredentialStore, RunnerCredentials,
};
use crate::protocol::client::{Client, ClientError, ErrorCategory, RunLogBuffer};
use crate::protocol::{
    FinishRunRequest, FinishRunResponse, FinishStatus, RegisterRunnerRequest, RunnerPollRequest,
    RunnerPollResponse,
};

const MAX_REGISTERED_CONCURRENCY: usize = 100;
const MAX_REGISTERED_COMMAND_LENGTH: usize = 1000;

/// A registered or reconnected local runner session.
///
/// The session holds the runner token in memory and uses it for runner
/// protocol requests. The bootstrap user API key is used only during first
/// registration and is never retained by this type.
#[derive(Clone)]
pub struct RunnerConnection {
    client: Client,
    credentials: RunnerCredentials,
    registered: bool,
}

impl RunnerConnection {
    /// Register this runner and atomically save its credentials to an
    /// operator-selected path.
    ///
    /// This always uses `TINES_API_KEY` for registration. It does not load or
    /// modify the credentials path from the runner configuration.
    pub fn register(config: &Config, output_path: &Path) -> Result<RunnerCredentials, RunnerError> {
        if !(1..=MAX_REGISTERED_CONCURRENCY).contains(&config.max_concurrent) {
            return Err(RunnerError::InvalidConcurrency(config.max_concurrent));
        }

        let request = registration_request(config)?;
        let client = Client::new(config.server_url.as_str()).map_err(RunnerError::ClientSetup)?;
        let key = BootstrapKey::from_env().map_err(RunnerError::BootstrapKey)?;
        let response = client
            .register_runner(key.as_str(), &request)
            .map_err(RunnerError::Registration)?;
        if response.runner.id.is_empty() || response.runner_token.is_empty() {
            return Err(RunnerError::InvalidRegistrationResponse);
        }

        let credentials = RunnerCredentials::new(response.runner.id, response.runner_token);
        CredentialStore::at(output_path)
            .save_atomic(&credentials)
            .map_err(|source| RunnerError::PersistRegisteredCredentials {
                path: output_path.to_path_buf(),
                source,
            })?;
        Ok(credentials)
    }

    /// Load credentials or register this runner when no credentials file
    /// exists. Malformed or unreadable credentials cause a hard failure.
    pub fn connect(config: &Config) -> Result<Self, RunnerError> {
        let client = Client::new(config.server_url.as_str()).map_err(RunnerError::ClientSetup)?;
        Self::connect_with_key_provider(config, client, BootstrapKey::from_env)
    }

    fn connect_with_key_provider<F>(
        config: &Config,
        client: Client,
        bootstrap_key: F,
    ) -> Result<Self, RunnerError>
    where
        F: FnOnce() -> Result<BootstrapKey, BootstrapKeyError>,
    {
        if !(1..=MAX_REGISTERED_CONCURRENCY).contains(&config.max_concurrent) {
            return Err(RunnerError::InvalidConcurrency(config.max_concurrent));
        }

        let store = CredentialStore::at(&config.credentials_file);
        match store.load() {
            Ok(credentials) => Ok(Self {
                client,
                credentials,
                registered: false,
            }),
            Err(CredentialError::Open { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                let request = registration_request(config)?;
                let key = bootstrap_key().map_err(RunnerError::BootstrapKey)?;
                let response = client
                    .register_runner(key.as_str(), &request)
                    .map_err(RunnerError::Registration)?;
                if response.runner.id.is_empty() || response.runner_token.is_empty() {
                    return Err(RunnerError::InvalidRegistrationResponse);
                }

                let credentials = RunnerCredentials::new(response.runner.id, response.runner_token);
                store.save(&credentials).map_err(RunnerError::Credentials)?;
                Ok(Self {
                    client,
                    credentials,
                    registered: true,
                })
            }
            Err(error) => Err(RunnerError::Credentials(error)),
        }
    }

    /// Return the credentials used by runner protocol calls.
    pub fn credentials(&self) -> &RunnerCredentials {
        &self.credentials
    }

    /// Return whether this start registered a new runner.
    pub fn registered(&self) -> bool {
        self.registered
    }

    /// Validate stored credentials without polling or claiming assignments.
    pub fn verify(&self) -> Result<(), RunnerError> {
        self.client
            .verify_runner_token(
                self.credentials.runner_id(),
                self.credentials.runner_token(),
            )
            .map_err(|error| self.protocol_error(error))
    }

    /// Poll Tines with the stored runner token.
    ///
    /// Authentication rejection is fatal. The connection does not attempt
    /// registration with `TINES_API_KEY` after Tines rejects this token.
    pub fn poll(&self, request: &RunnerPollRequest) -> Result<RunnerPollResponse, RunnerError> {
        self.client
            .poll_runner(
                self.credentials.runner_id(),
                self.credentials.runner_token(),
                request,
            )
            .map_err(|error| self.protocol_error(error))
    }

    /// Report a local assignment preparation failure.
    pub fn finish_failed_assignment(
        &self,
        run_id: &str,
        error: &str,
    ) -> Result<FinishRunResponse, RunnerError> {
        self.finish_assignment(
            run_id,
            &FinishRunRequest {
                status: FinishStatus::Failed,
                error: Some(error.to_owned()),
                provider_session_id: None,
                usage: None,
                pricing_evidence: None,
                judgment: None,
                resume_at: None,
            },
        )
    }

    /// Report an ordinary assignment outcome with any observed provider data.
    pub fn finish_assignment(
        &self,
        run_id: &str,
        request: &FinishRunRequest,
    ) -> Result<FinishRunResponse, RunnerError> {
        self.client
            .finish_run(run_id, self.credentials.runner_token(), request)
            .map_err(|error| self.protocol_error(error))
    }

    /// Flush and close run logs before reporting an ordinary terminal status.
    /// Callers handling supervisor cancellation must stop log delivery and
    /// omit the finish request instead.
    pub fn finish_run_with_logs(
        &self,
        run_id: &str,
        logs: &RunLogBuffer,
        status: FinishStatus,
        error: Option<&str>,
    ) -> Result<FinishRunResponse, RunnerError> {
        logs.flush_before_finish(&self.client, run_id, self.credentials.runner_token())
            .map_err(|error| self.protocol_error(error))?;
        self.finish_assignment(
            run_id,
            &FinishRunRequest {
                status,
                error: error.map(str::to_owned),
                provider_session_id: None,
                usage: None,
                pricing_evidence: None,
                judgment: None,
                resume_at: None,
            },
        )
    }

    /// Flush logs before reporting a finish request with provider evidence.
    pub fn finish_assignment_with_logs(
        &self,
        run_id: &str,
        logs: &RunLogBuffer,
        request: &FinishRunRequest,
    ) -> Result<Option<FinishRunResponse>, RunnerError> {
        logs.flush_before_finish(&self.client, run_id, self.credentials.runner_token())
            .map_err(|error| self.protocol_error(error))?;
        if logs.is_cancelled() {
            return Ok(None);
        }
        self.finish_assignment(run_id, request).map(Some)
    }

    fn protocol_error(&self, error: ClientError) -> RunnerError {
        if error.category() == ErrorCategory::Authentication {
            RunnerError::RejectedRunnerToken {
                runner_id: self.credentials.runner_id().to_owned(),
            }
        } else if error.category() == ErrorCategory::Fencing {
            RunnerError::Superseded {
                runner_id: self.credentials.runner_id().to_owned(),
            }
        } else {
            RunnerError::Protocol(error)
        }
    }
}

fn registration_request(config: &Config) -> Result<RegisterRunnerRequest, RunnerError> {
    let harness = config.runner_type.tines_harness();
    let command = match config.runner_type {
        RunnerType::Codex | RunnerType::Antigravity => None,
        RunnerType::Custom => Some(render_registered_command(
            config
                .custom_command
                .as_deref()
                .ok_or(RunnerError::MissingCustomCommand)?,
        )?),
    };

    Ok(RegisterRunnerRequest {
        name: config.runner_name.clone(),
        harness: Some(harness),
        command,
        max_concurrent: Some(config.max_concurrent as u32),
        max_run_minutes: None,
        hostname: local_hostname(),
        platform: Some(format!(
            "{} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    })
}

/// Render argv as POSIX shell-style metadata. The result is sent only as
/// registration/display data; execution continues to use the original argv.
fn render_registered_command(arguments: &[String]) -> Result<String, RunnerError> {
    if arguments.is_empty()
        || arguments[0].trim().is_empty()
        || arguments.iter().any(|arg| arg.contains('\0'))
    {
        return Err(RunnerError::InvalidCustomCommand);
    }

    let mut rendered = String::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            rendered.push(' ');
        }
        if !argument.is_empty() && argument.chars().all(is_shell_safe_unquoted) {
            rendered.push_str(argument);
        } else {
            rendered.push('\'');
            for character in argument.chars() {
                if character == '\'' {
                    rendered.push_str("'\\''");
                } else {
                    rendered.push(character);
                }
            }
            rendered.push('\'');
        }
    }

    let length = rendered.encode_utf16().count();
    if length > MAX_REGISTERED_COMMAND_LENGTH {
        return Err(RunnerError::RegisteredCommandTooLong {
            length,
            limit: MAX_REGISTERED_COMMAND_LENGTH,
        });
    }
    Ok(rendered)
}

fn is_shell_safe_unquoted(character: char) -> bool {
    character.is_ascii_alphanumeric() || "_@%+=:,./-".contains(character)
}

fn local_hostname() -> Option<String> {
    ["HOSTNAME", "COMPUTERNAME"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| {
            !value.is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
        })
}

/// A failure while registering or reconnecting a local runner.
#[derive(Debug)]
pub enum RunnerError {
    ClientSetup(ClientError),
    Credentials(CredentialError),
    BootstrapKey(BootstrapKeyError),
    Registration(ClientError),
    PersistRegisteredCredentials {
        path: PathBuf,
        source: CredentialError,
    },
    InvalidConcurrency(usize),
    MissingCustomCommand,
    InvalidCustomCommand,
    RegisteredCommandTooLong {
        length: usize,
        limit: usize,
    },
    InvalidRegistrationResponse,
    RejectedRunnerToken {
        runner_id: String,
    },
    Superseded {
        runner_id: String,
    },
    Protocol(ClientError),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientSetup(error) => write!(f, "could not create the Tines client: {error}"),
            Self::Credentials(error) => write!(f, "could not use runner credentials: {error}"),
            Self::BootstrapKey(error) => {
                write!(f, "no stored runner credentials; cannot register: {error}")
            }
            Self::Registration(error) => write!(f, "could not register the local runner: {error}"),
            Self::PersistRegisteredCredentials { path, source } => write!(
                f,
                "runner registration succeeded, but credentials could not be persisted to {}: {source}",
                path.display()
            ),
            Self::InvalidConcurrency(value) => write!(
                f,
                "runner max_concurrent must be between 1 and {MAX_REGISTERED_CONCURRENCY} (got {value})"
            ),
            Self::MissingCustomCommand => {
                f.write_str("custom runner registration requires a configured custom_command")
            }
            Self::InvalidCustomCommand => f.write_str(
                "custom runner registration requires a non-empty executable argument and arguments without null bytes",
            ),
            Self::RegisteredCommandTooLong { length, limit } => write!(
                f,
                "rendered custom command is {length} UTF-16 code units; Tines supports at most {limit}"
            ),
            Self::InvalidRegistrationResponse => {
                f.write_str("Tines returned an empty runner ID or runner token")
            }
            Self::RejectedRunnerToken { runner_id } => write!(
                f,
                "Tines rejected the runner token for runner {runner_id}; stopped without falling back to TINES_API_KEY. Verify the credentials file and register the runner again if needed."
            ),
            Self::Superseded { runner_id } => write!(
                f,
                "Tines superseded this daemon for runner {runner_id}; exiting"
            ),
            Self::Protocol(error) => write!(f, "Tines runner protocol request failed: {error}"),
        }
    }
}

impl Error for RunnerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ClientSetup(error) | Self::Registration(error) | Self::Protocol(error) => {
                Some(error)
            }
            Self::Credentials(error) => Some(error),
            Self::PersistRegisteredCredentials { source, .. } => Some(source),
            Self::BootstrapKey(error) => Some(error),
            Self::InvalidConcurrency(_)
            | Self::MissingCustomCommand
            | Self::InvalidCustomCommand
            | Self::RegisteredCommandTooLong { .. }
            | Self::InvalidRegistrationResponse
            | Self::RejectedRunnerToken { .. } => None,
            Self::Superseded { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_REGISTERED_COMMAND_LENGTH, RunnerConnection, RunnerError, registration_request,
        render_registered_command,
    };
    use crate::config::Config;
    use crate::credentials::{BootstrapKey, CredentialStore, RunnerCredentials};
    use crate::protocol::client::{Client, RunLogBuffer};
    use crate::protocol::{FinishStatus, RunnerPollRequest};
    use serde_json::Value;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::{self, JoinHandle};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tines-runner-registration-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn credentials_path(&self) -> PathBuf {
            self.0.join("credentials.toml")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(server_url: &str, credentials_file: &std::path::Path) -> Config {
        Config::from_toml_str(&format!(
            "[server]\nurl = \"{server_url}\"\n[runner]\nname = \"codex-test\"\nexecutor_cwd = \"~\"\nmax_concurrent = 3\n[storage]\ncredentials_file = \"{}\"\n",
            credentials_file.display()
        ))
        .expect("valid runner config")
    }

    fn mock_response(status: u16, body: &'static str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
        let address = listener.local_addr().expect("read mock address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Tines request");
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = stream.read(&mut chunk).expect("read Tines request");
                assert_ne!(count, 0, "client closed before sending the request");
                request.extend_from_slice(&chunk[..count]);
                if request_complete(&request) {
                    break;
                }
            }

            let reason = match status {
                200 => "OK",
                201 => "Created",
                400 => "Bad Request",
                401 => "Unauthorized",
                _ => "Mock",
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write mock response");
            String::from_utf8(request).expect("request should be UTF-8")
        });
        (format!("http://{address}"), server)
    }

    fn request_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let Ok(headers) = std::str::from_utf8(&request[..header_end]) else {
            return false;
        };
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        request.len() >= header_end + 4 + content_length
    }

    fn request_parts(request: &str) -> (&str, &str) {
        let (headers, body) = request.split_once("\r\n\r\n").expect("request headers");
        (headers, body)
    }

    fn request_json(request: &str) -> Value {
        let (_, body) = request_parts(request);
        serde_json::from_str(body).expect("JSON request body")
    }

    #[test]
    fn fresh_registration_saves_only_the_returned_runner_credentials() {
        let directory = TestDirectory::new();
        let (url, server) = mock_response(
            201,
            r#"{"runner":{"id":"rnr_registered"},"runner_token":"runner-secret"}"#,
        );
        let config = config(&url, &directory.credentials_path());
        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || BootstrapKey::from_value("user-api-secret"),
        )
        .expect("runner registration");

        assert!(connection.registered());
        assert_eq!(connection.credentials().runner_id(), "rnr_registered");
        assert_eq!(connection.credentials().runner_token(), "runner-secret");

        let request = server.join().expect("mock request");
        let (headers, _) = request_parts(&request);
        assert!(headers.contains("POST /api/v1/runners/register HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer user-api-secret")
        );
        let body = request_json(&request);
        assert_eq!(body["name"], "codex-test");
        assert_eq!(body["harness"], "codex");
        assert!(body.get("command").is_none());
        assert_eq!(body["max_concurrent"], 3);
        assert_eq!(
            body["platform"],
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
        );
        if let Some(hostname) = body["hostname"].as_str() {
            assert!(!hostname.is_empty());
        }

        let saved = fs::read_to_string(directory.credentials_path()).expect("saved credentials");
        assert!(saved.contains("rnr_registered"));
        assert!(saved.contains("runner-secret"));
        assert!(!saved.contains("user-api-secret"));
    }

    #[test]
    fn custom_runner_registers_with_the_custom_semantic_harness() {
        let directory = TestDirectory::new();
        let config = Config::from_toml_str(&format!(
            "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"checks-runner\"\nrunner_type = \"custom\"\ncustom_command = [\"checks\"]\nexecutor_cwd = \"~\"\n[storage]\ncredentials_file = {:?}\n",
            directory.credentials_path()
        ))
        .expect("custom runner config");

        let request =
            serde_json::to_value(registration_request(&config).expect("registration request"))
                .expect("serialize registration request");
        assert_eq!(request["harness"], "custom");
        assert_eq!(request["name"], "checks-runner");
        assert_eq!(request["command"], "checks");
    }

    #[test]
    fn antigravity_registers_as_pi_without_a_custom_command() {
        let directory = TestDirectory::new();
        let config = Config::from_toml_str(&format!(
            "[server]\nurl = \"https://tines.example.test\"\n[runner]\nname = \"agy-runner\"\nrunner_type = \"antigravity\"\nexecutor_cwd = \"~\"\n[storage]\ncredentials_file = {:?}\n",
            directory.credentials_path()
        ))
        .expect("Antigravity runner config");

        let request =
            serde_json::to_value(registration_request(&config).expect("registration request"))
                .expect("serialize registration request");
        assert_eq!(request["harness"], "pi");
        assert!(request.get("command").is_none());
    }

    #[test]
    fn custom_registration_sends_a_quoted_display_command() {
        let directory = TestDirectory::new();
        let (url, server) = mock_response(
            201,
            r#"{"runner":{"id":"rnr_custom"},"runner_token":"runner-secret"}"#,
        );
        let mut config = config(&url, &directory.credentials_path());
        config.runner_name = "checks-runner".to_owned();
        config.runner_type = crate::config::RunnerType::Custom;
        config.custom_command = Some(vec![
            "node".to_owned(),
            "/home/user/github-status-checks.mjs".to_owned(),
            "--prompt".to_owned(),
            "{prompt_file}".to_owned(),
            "--workspace".to_owned(),
            "{workspace}".to_owned(),
            "value with spaces".to_owned(),
            "it's $HOME".to_owned(),
        ]);

        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || BootstrapKey::from_value("user-api-secret"),
        )
        .expect("custom runner registration");
        assert!(connection.registered());

        let request = server.join().expect("mock registration request");
        let body = request_json(&request);
        assert_eq!(body["harness"], "custom");
        assert_eq!(
            body["command"],
            "node /home/user/github-status-checks.mjs --prompt '{prompt_file}' --workspace '{workspace}' 'value with spaces' 'it'\\''s $HOME'"
        );
    }

    #[test]
    fn custom_registration_reports_tines_validation_messages() {
        let directory = TestDirectory::new();
        let (url, server) = mock_response(
            422,
            r#"{"error":{"code":"invalid_field","message":"The custom harness needs a config.command template"}}"#,
        );
        let mut config = config(&url, &directory.credentials_path());
        config.runner_type = crate::config::RunnerType::Custom;
        config.custom_command = Some(vec!["checks".to_owned()]);

        let error = match RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || BootstrapKey::from_value("user-api-secret"),
        ) {
            Ok(_) => panic!("validation failure should be reported"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("HTTP request failed with status 422")
        );
        assert!(
            error
                .to_string()
                .contains("The custom harness needs a config.command template")
        );
        let request = server.join().expect("mock registration request");
        assert_eq!(request_json(&request)["harness"], "custom");
    }

    #[test]
    fn overlong_custom_registration_command_fails_locally() {
        let directory = TestDirectory::new();
        let mut config = config("http://127.0.0.1:1", &directory.credentials_path());
        config.runner_type = crate::config::RunnerType::Custom;
        config.custom_command = Some(vec!["x".repeat(MAX_REGISTERED_COMMAND_LENGTH + 1)]);

        let error = match RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(config.server_url.as_str()).expect("Tines client"),
            || panic!("an invalid command must fail before reading the bootstrap key"),
        ) {
            Ok(_) => panic!("overlong command must fail before registration"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            RunnerError::RegisteredCommandTooLong {
                length: 1001,
                limit: 1000
            }
        ));
        assert!(error.to_string().contains("Tines supports at most 1000"));
    }

    #[test]
    fn registered_command_renderer_quotes_shell_special_characters_and_empty_args() {
        let command = render_registered_command(&[
            "node".to_owned(),
            "two words".to_owned(),
            "it's".to_owned(),
            String::new(),
            "$(echo hi)".to_owned(),
        ])
        .expect("render command");

        assert_eq!(command, "node 'two words' 'it'\\''s' '' '$(echo hi)'");
    }

    #[test]
    fn registered_command_length_matches_tines_utf16_limit() {
        let command =
            render_registered_command(&["😀".repeat((MAX_REGISTERED_COMMAND_LENGTH - 2) / 2)])
                .expect("1000 UTF-16 code units are accepted");
        assert_eq!(
            command.encode_utf16().count(),
            MAX_REGISTERED_COMMAND_LENGTH
        );

        let error = render_registered_command(&["😀".repeat(MAX_REGISTERED_COMMAND_LENGTH / 2)])
            .expect_err("1002 UTF-16 code units exceed the limit");
        assert!(matches!(
            error,
            RunnerError::RegisteredCommandTooLong {
                length: 1002,
                limit: 1000
            }
        ));
    }

    #[test]
    fn reconnect_uses_saved_token_without_a_bootstrap_key() {
        let directory = TestDirectory::new();
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_saved", "saved-runner-token"))
            .expect("write saved credentials");
        let (url, server) = mock_response(200, r#"{"assignments":[],"cancels":[]}"#);
        let config = config(&url, store.path());
        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || panic!("a saved token must not require a bootstrap key"),
        )
        .expect("runner reconnect");

        assert!(!connection.registered());
        let response = connection
            .poll(&RunnerPollRequest::default())
            .expect("authenticated poll");
        assert!(response.assignments.is_empty());
        let request = server.join().expect("mock request");
        let (headers, _) = request_parts(&request);
        assert!(headers.contains("POST /api/v1/runners/rnr_saved/poll HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer saved-runner-token")
        );
    }

    #[test]
    fn ordinary_finish_flushes_the_last_log_batch_before_reporting_status() {
        let directory = TestDirectory::new();
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_saved", "saved-runner-token"))
            .expect("write saved credentials");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
        let address = listener.local_addr().expect("read mock address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (response_body, reason) in [
                (
                    r#"{"status":"running","log_bytes_dropped":0,"log_seq":1}"#,
                    "OK",
                ),
                (r#"{"id":"arun_1","status":"completed"}"#, "OK"),
            ] {
                let (mut stream, _) = listener.accept().expect("accept Tines request");
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let count = stream.read(&mut chunk).expect("read Tines request");
                    assert_ne!(count, 0, "client closed before sending the request");
                    request.extend_from_slice(&chunk[..count]);
                    if request_complete(&request) {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).expect("request should be UTF-8"));
                write!(
                    stream,
                    "HTTP/1.1 200 {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                )
                .expect("write Tines response");
            }
            requests
        });

        let url = format!("http://{address}");
        let config = config(&url, store.path());
        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || panic!("saved credentials do not need a bootstrap key"),
        )
        .expect("load saved runner credentials");
        let logs = RunLogBuffer::new();
        assert!(
            logs.append_harness_output(
                &Client::new(&url).expect("log client"),
                "arun_1",
                "saved-runner-token",
                "final output\n",
            )
            .expect("buffer final harness output")
            .is_none()
        );

        connection
            .finish_run_with_logs("arun_1", &logs, FinishStatus::Completed, None)
            .expect("flush logs and finish run");
        let requests = server.join().expect("join mock Tines server");
        assert!(requests[0].contains("POST /api/v1/runs/arun_1/logs HTTP/1.1"));
        assert_eq!(request_json(&requests[0])["chunk"], "final output\n");
        assert_eq!(request_json(&requests[0])["seq"], 1);
        assert!(requests[1].contains("POST /api/v1/runs/arun_1/finish HTTP/1.1"));
        assert_eq!(request_json(&requests[1])["status"], "completed");
        assert!(logs.is_stopped());
    }

    #[test]
    fn startup_verifies_saved_token_with_a_malformed_poll_body() {
        let directory = TestDirectory::new();
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_saved", "saved-runner-token"))
            .expect("write saved credentials");
        let (url, server) = mock_response(
            400,
            r#"{"error":{"code":"invalid_json","message":"Request body must be valid JSON"}}"#,
        );
        let config = config(&url, store.path());
        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || panic!("a saved token must not require a bootstrap key"),
        )
        .expect("load saved runner credentials");

        connection.verify().expect("validate saved runner token");
        let request = server.join().expect("mock request");
        let (headers, body) = request_parts(&request);
        assert!(headers.contains("POST /api/v1/runners/rnr_saved/poll HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer saved-runner-token")
        );
        assert_eq!(body, "{");
    }

    #[test]
    fn rejected_saved_token_fails_closed_without_bootstrap_fallback() {
        let directory = TestDirectory::new();
        let store = CredentialStore::at(directory.credentials_path());
        store
            .save(&RunnerCredentials::new("rnr_rejected", "rejected-token"))
            .expect("write saved credentials");
        let (url, server) = mock_response(
            401,
            r#"{"error":{"code":"runner_token_invalid","message":"Invalid token"}}"#,
        );
        let config = config(&url, store.path());
        let connection = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(&url).expect("Tines client"),
            || panic!("a rejected saved token must not fall back to a bootstrap key"),
        )
        .expect("load saved runner credentials");

        let error = match connection.verify() {
            Ok(()) => panic!("rejected token should stop startup"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Tines rejected the runner token for runner rnr_rejected; stopped without falling back to TINES_API_KEY. Verify the credentials file and register the runner again if needed."
        );
        let request = server.join().expect("mock request");
        let (headers, _) = request_parts(&request);
        assert!(headers.contains("POST /api/v1/runners/rnr_rejected/poll HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer rejected-token")
        );
    }

    #[test]
    fn missing_credentials_require_the_bootstrap_key() {
        let directory = TestDirectory::new();
        let url = "http://127.0.0.1:1";
        let config = config(url, &directory.credentials_path());
        let result = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(url).expect("Tines client"),
            || Err(crate::credentials::BootstrapKeyError::Missing),
        );
        let error = match result {
            Ok(_) => panic!("missing bootstrap key should fail"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "no stored runner credentials; cannot register: TINES_API_KEY is not set"
        );
    }

    #[test]
    fn malformed_credentials_fail_without_registration_fallback() {
        let directory = TestDirectory::new();
        fs::write(directory.credentials_path(), "not valid TOML = [")
            .expect("write malformed credentials");
        let url = "http://127.0.0.1:1";
        let config = config(url, &directory.credentials_path());
        let result = RunnerConnection::connect_with_key_provider(
            &config,
            Client::new(url).expect("Tines client"),
            || panic!("malformed credentials must not trigger registration"),
        );

        let error = match result {
            Ok(_) => panic!("malformed credentials should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("credentials file"));
        assert!(error.to_string().contains("malformed TOML"));
    }

    #[test]
    fn rejected_token_error_has_a_stable_operator_diagnostic() {
        let error = RunnerError::RejectedRunnerToken {
            runner_id: "rnr_fixed".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "Tines rejected the runner token for runner rnr_fixed; stopped without falling back to TINES_API_KEY. Verify the credentials file and register the runner again if needed."
        );
    }
}
