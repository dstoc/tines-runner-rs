//! Child-process supervision and termination.
//!
//! On Unix, each command runs in a fresh process group. Termination signals the
//! group so wrappers and their descendants receive the same shutdown request.
//! On Linux, [`ProcessIdentity`] also includes the boot ID and process start
//! time, which lets recovery distinguish a live child from a reused PID.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};

/// Identifies a launched process and, where available, its process group.
///
/// On Linux, `boot_id` and `start_time_ticks` form a process-generation token.
/// They must be checked with [`ProcessIdentity::matches_live_process`] before
/// using the process or group IDs during crash recovery.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProcessIdentity {
    process_id: u32,
    process_group_id: Option<u32>,
    boot_id: Option<String>,
    start_time_ticks: Option<u64>,
}

impl ProcessIdentity {
    /// Return the direct child's process ID.
    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    /// Return the process-group ID, when this platform uses process groups.
    pub fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    /// Check that the same process generation is still running.
    ///
    /// Linux compares the boot ID, process start time, and process group. On
    /// other platforms this returns `false` because this implementation does
    /// not have a generation-safe process query there.
    pub fn matches_live_process(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let (boot_id, start_time_ticks, process_group_id) =
                match linux_process_details(self.process_id) {
                    Ok(details) => details,
                    Err(_) => return false,
                };
            self.boot_id.as_deref() == Some(boot_id.as_str())
                && self.start_time_ticks == Some(start_time_ticks)
                && self.process_group_id == Some(process_group_id)
        }

        #[cfg(not(target_os = "linux"))]
        false
    }

    fn for_child(process_id: u32) -> Self {
        #[cfg(unix)]
        let process_group_id = Some(process_id);
        #[cfg(not(unix))]
        let process_group_id = None;

        #[cfg(target_os = "linux")]
        let (boot_id, start_time_ticks) = linux_process_details(process_id)
            .map(|(boot_id, start_time_ticks, _)| (Some(boot_id), Some(start_time_ticks)))
            .unwrap_or((None, None));
        #[cfg(not(target_os = "linux"))]
        let (boot_id, start_time_ticks) = (None, None);

        Self {
            process_id,
            process_group_id,
            boot_id,
            start_time_ticks,
        }
    }
}

/// The result of a supervised process, including both captured output streams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    /// The direct child's exit code or terminating signal.
    pub exit: ProcessExit,
    /// Whether `wait_timeout` reached its deadline and terminated the group.
    pub timed_out: bool,
    /// Bytes written to stdout by the child and its descendants.
    pub stdout: Vec<u8>,
    /// Bytes written to stderr by the child and its descendants.
    pub stderr: Vec<u8>,
}

/// A portable description of how the direct child ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExit {
    /// The child returned an exit code.
    Code(i32),
    /// Unix reports the signal that ended the child.
    Signal(i32),
    /// The platform did not provide an exit code or signal.
    Unknown,
}

/// A running command whose output and process tree are owned by this handle.
pub struct SupervisedProcess {
    child: Child,
    identity: ProcessIdentity,
    stdout: JoinHandle<io::Result<Vec<u8>>>,
    stderr: JoinHandle<io::Result<Vec<u8>>>,
}

impl SupervisedProcess {
    /// Start a command with piped stdout and stderr.
    ///
    /// Unix children start in a new process group. The returned identity is
    /// captured before this method returns, so callers can persist it before
    /// awaiting the command.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let identity = ProcessIdentity::for_child(child.id());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout pipe was not created"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr pipe was not created"))?;

        let stdout = match thread::Builder::new()
            .name("runner-child-stdout".to_owned())
            .spawn(move || read_to_end(stdout))
        {
            Ok(stdout) => stdout,
            Err(error) => {
                #[cfg(unix)]
                let _ = signal_unix_group(child.id(), SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stderr = match thread::Builder::new()
            .name("runner-child-stderr".to_owned())
            .spawn(move || read_to_end(stderr))
        {
            Ok(stderr) => stderr,
            Err(error) => {
                #[cfg(unix)]
                let _ = signal_unix_group(child.id(), SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                return Err(error);
            }
        };

        Ok(Self {
            child,
            identity,
            stdout,
            stderr,
        })
    }

    /// Return the process identity captured at spawn time.
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    /// Wait for the direct child, clean up any remaining group members, and
    /// collect stdout and stderr.
    pub fn wait(mut self) -> io::Result<ProcessOutput> {
        let status = self.child.wait()?;
        self.terminate_remaining_group(Duration::from_secs(2))?;
        self.collect(status, false)
    }

    /// Wait up to `timeout`, then terminate the process group with SIGTERM and
    /// SIGKILL after `grace` if the group remains alive.
    pub fn wait_timeout(mut self, timeout: Duration, grace: Duration) -> io::Result<ProcessOutput> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.terminate_remaining_group(grace)?;
                return self.collect(status, false);
            }
            let now = Instant::now();
            if now >= deadline {
                let status = self.terminate_group(grace)?;
                return self.collect(status, true);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }

    /// Terminate the process group, wait for the direct child, and collect its
    /// output. The process receives a graceful signal before forced termination.
    pub fn terminate(mut self, grace: Duration) -> io::Result<ProcessOutput> {
        let status = self.terminate_group(grace)?;
        self.collect(status, false)
    }

    fn terminate_remaining_group(&mut self, grace: Duration) -> io::Result<()> {
        #[cfg(unix)]
        {
            if unix_group_exists(self.child.id())? {
                let _ = self.terminate_group(grace)?;
            }
        }
        #[cfg(not(unix))]
        let _ = grace;
        Ok(())
    }

    fn terminate_group(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            signal_unix_group(self.child.id(), SIGTERM)?;
            let deadline = Instant::now() + grace;
            let mut status = self.child.try_wait()?;
            while Instant::now() < deadline && unix_group_exists(self.child.id())? {
                if status.is_none() {
                    status = self.child.try_wait()?;
                }
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            if unix_group_exists(self.child.id())? {
                signal_unix_group(self.child.id(), SIGKILL)?;
            }
            match status {
                Some(status) => Ok(status),
                None => self.child.wait(),
            }
        }

        #[cfg(not(unix))]
        {
            let _ = grace;
            let _ = self.child.kill();
            self.child.wait()
        }
    }

    fn collect(self, status: ExitStatus, timed_out: bool) -> io::Result<ProcessOutput> {
        let stdout = join_reader(self.stdout)?;
        let stderr = join_reader(self.stderr)?;
        Ok(ProcessOutput {
            exit: process_exit(status),
            timed_out,
            stdout,
            stderr,
        })
    }
}

fn read_to_end(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    Ok(output)
}

fn join_reader(reader: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("child output reader panicked"))?
}

fn process_exit(status: ExitStatus) -> ProcessExit {
    if let Some(code) = status.code() {
        return ProcessExit::Code(code);
    }
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return ProcessExit::Signal(signal);
    }
    ProcessExit::Unknown
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(unix)]
fn signal_unix_group(process_group_id: u32, signal: i32) -> io::Result<()> {
    // A negative PID addresses the process group created for this child.
    let result = unsafe { kill(-(process_group_id as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(3) => Ok(()), // ESRCH: the group has already exited.
        _ => Err(error),
    }
}

#[cfg(unix)]
fn unix_group_exists(process_group_id: u32) -> io::Result<bool> {
    let result = unsafe { kill(-(process_group_id as i32), 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(3) => Ok(false), // ESRCH
        Some(1) => Ok(true),  // EPERM: a group exists but is not signalable.
        _ => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn linux_process_details(process_id: u32) -> io::Result<(String, u64, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{process_id}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat record"))?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let process_group_id = fields
        .get(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process group ID"))?
        .parse::<u32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let start_time_ticks = fields
        .get(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    Ok((boot_id, start_time_ticks, process_group_id))
}

#[cfg(test)]
mod tests {
    use super::{ProcessExit, SupervisedProcess};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tines-runner-process-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn captures_both_streams_and_reports_exit_code() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'out'; printf 'err' >&2; exit 23"]);
        let result = SupervisedProcess::spawn(&mut command)
            .expect("spawn harness")
            .wait()
            .expect("wait for harness");

        assert_eq!(result.exit, ProcessExit::Code(23));
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"err");
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn reports_signal_separately_from_exit_code() {
        let mut command = Command::new("sh");
        command.args(["-c", "kill -TERM $$"]);
        let result = SupervisedProcess::spawn(&mut command)
            .expect("spawn harness")
            .wait()
            .expect("wait for harness");

        assert_eq!(result.exit, ProcessExit::Signal(15));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendants_after_the_grace_period() {
        let directory = TestDirectory::new();
        let pid_path = directory.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "trap '' TERM; (trap '' TERM; exec sleep 30) & echo $! > \"$1\"; wait",
            "stub-harness",
        ]);
        command.arg(&pid_path);

        let process = SupervisedProcess::spawn(&mut command).expect("spawn harness");
        assert!(process.identity().matches_live_process());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let descendant = fs::read_to_string(&pid_path)
            .expect("harness wrote descendant PID")
            .trim()
            .parse::<u32>()
            .expect("valid descendant PID");

        let result = process
            .wait_timeout(Duration::from_millis(100), Duration::from_millis(100))
            .expect("terminate timed-out process group");
        assert!(result.timed_out);
        assert_eq!(result.exit, ProcessExit::Signal(9));
        assert_descendant_stopped(descendant);
    }

    #[cfg(target_os = "linux")]
    fn assert_descendant_stopped(process_id: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs::read_to_string(format!("/proc/{process_id}/stat")) {
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(") ")
                        .expect("valid proc stat record")
                        .1
                        .chars()
                        .next()
                        .expect("process state");
                    if state == 'Z' {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => panic!("could not inspect descendant: {error}"),
            }
            assert!(Instant::now() < deadline, "descendant remained alive");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn assert_descendant_stopped(process_id: u32) {
        let mut command = Command::new("kill");
        command.args(["-0", &process_id.to_string()]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !command.status().expect("run kill probe").success() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant remained alive");
    }
}
