use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tines-runner-cli-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn config_dir(&self) -> PathBuf {
        self.0.join("config")
    }

    fn credentials_path(&self) -> PathBuf {
        self.0.join("credentials.toml")
    }

    fn configure_command(&self, command: &mut Command) {
        let config_dir = self.config_dir();
        command
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir)
            .env("LOCALAPPDATA", &config_dir);
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read HTTP request");
        assert_ne!(count, 0, "client closed before sending the request");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("request headers");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            return String::from_utf8(request).expect("HTTP request should be UTF-8");
        }
    }
}

fn registration_server() -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Tines server");
    let address = listener.local_addr().expect("read mock address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Tines request");
        let request = read_http_request(&mut stream);
        let response = r#"{"runner":{"id":"rnr_cli_test"},"runner_token":"cli-runner-token"}"#;
        write!(
            stream,
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .expect("write registration response");
        request
    });
    (format!("http://{address}"), server)
}

#[test]
fn version_flag_reports_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
        .arg("--version")
        .output()
        .expect("runner binary should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("tines-runner-rs {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn startup_registers_persists_credentials_and_restarts_without_bootstrap_key() {
    let directory = TestDirectory::new();
    let config_dir = directory.config_dir().join("tines-runner-rs");
    fs::create_dir_all(&config_dir).expect("create runner config directory");
    let (server_url, server) = registration_server();
    let credentials_path =
        toml::Value::String(directory.credentials_path().to_string_lossy().into_owned());
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[server]\nurl = {server_url:?}\n[runner]\nname = \"cli-test-runner\"\nmax_concurrent = 2\n[storage]\ncredentials_file = {credentials_path}\n"
        ),
    )
    .expect("write runner config");

    let mut first_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut first_start);
    let first_start = first_start
        .env("TINES_API_KEY", "bootstrap-key-test")
        .output()
        .expect("start runner for registration");
    let request = server.join().expect("registration server request");

    assert!(
        first_start.status.success(),
        "startup should register the runner: {}",
        String::from_utf8_lossy(&first_start.stderr)
    );
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("registration request headers");
    assert!(headers.contains("POST /api/v1/runners/register HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer bootstrap-key-test")
    );
    let body: serde_json::Value = serde_json::from_str(body).expect("registration request JSON");
    assert_eq!(body["name"], "cli-test-runner");
    assert_eq!(body["harness"], "codex");
    assert_eq!(body["max_concurrent"], 2);

    let saved_credentials = fs::read_to_string(directory.credentials_path())
        .expect("read persisted runner credentials");
    assert!(saved_credentials.contains("rnr_cli_test"));
    assert!(saved_credentials.contains("cli-runner-token"));
    assert!(!saved_credentials.contains("bootstrap-key-test"));

    let mut second_start = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"));
    directory.configure_command(&mut second_start);
    let second_start = second_start
        .env_remove("TINES_API_KEY")
        .output()
        .expect("restart runner from persisted credentials");
    assert!(
        second_start.status.success(),
        "restart should use saved credentials without TINES_API_KEY: {}",
        String::from_utf8_lossy(&second_start.stderr)
    );
}
