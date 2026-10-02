//! Child-process supervision and termination.
//!
//! On Unix, each command runs in a fresh process group. On Windows, the
//! suspended child is assigned to a Job Object before its main thread resumes.
//! Both mechanisms contain wrappers and their descendants. Linux identities
//! include the boot ID and start time; Windows identities include process
//! creation time to reject a reused PID.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Identifies a launched process and, where available, its process group.
///
/// Linux uses `boot_id` and `start_time_ticks`; Windows uses the process
/// creation time in `start_time_ticks`. Check the generation with
/// [`ProcessIdentity::matches_live_process`] before using the process or group
/// IDs during crash recovery.
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
    /// Linux compares the boot ID, process start time, and process group.
    /// Windows compares process creation time. On other platforms this returns
    /// `false` because this implementation does not have a generation-safe
    /// process query there.
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

        #[cfg(windows)]
        {
            self.start_time_ticks.is_some()
                && self.process_group_id == Some(self.process_id)
                && windows_process_creation_time(self.process_id).ok() == self.start_time_ticks
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        false
    }

    fn for_child(child: &Child) -> Self {
        let process_id = child.id();
        #[cfg(unix)]
        let process_group_id = Some(process_id);
        #[cfg(windows)]
        let process_group_id = Some(process_id);
        #[cfg(not(any(unix, windows)))]
        let process_group_id = None;

        #[cfg(target_os = "linux")]
        let (boot_id, start_time_ticks) = linux_process_details(process_id)
            .map(|(boot_id, start_time_ticks, _)| (Some(boot_id), Some(start_time_ticks)))
            .unwrap_or((None, None));
        #[cfg(all(not(target_os = "linux"), not(windows)))]
        let (boot_id, start_time_ticks) = (None, None);
        #[cfg(windows)]
        let boot_id = None;
        #[cfg(windows)]
        let start_time_ticks = windows_process_creation_time_from_handle(child).ok();

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
    #[cfg(windows)]
    job: WindowsJob,
}

impl SupervisedProcess {
    /// Start a command with piped stdout and stderr.
    ///
    /// Unix children start in a new process group. On Windows, the main thread
    /// starts suspended and resumes only after the process joins a Job Object.
    /// The returned identity is captured before this method returns, so callers
    /// can persist it before awaiting the command.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(windows)]
        let job = WindowsJob::new()?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(WINDOWS_CREATE_SUSPENDED | WINDOWS_CREATE_NEW_PROCESS_GROUP);

        let mut child = command.spawn()?;
        let identity = ProcessIdentity::for_child(&child);
        #[cfg(windows)]
        if let Err(error) = job.assign_and_resume(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
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
                #[cfg(windows)]
                let _ = job.terminate();
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
                #[cfg(windows)]
                let _ = job.terminate();
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
            #[cfg(windows)]
            job,
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

    /// Wait up to `timeout`, then request graceful group termination and force
    /// termination after `grace` if the group remains alive.
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

    /// Terminate the process tree, wait for the direct child, and collect its
    /// output. The process tree receives a graceful request before forced
    /// termination.
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
        {
            #[cfg(windows)]
            if self.job.active_processes()? > 0 {
                let _ = self.terminate_group(grace)?;
            }
            #[cfg(not(windows))]
            let _ = grace;
        }
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
            #[cfg(windows)]
            {
                let _ = windows_generate_console_ctrl_break(self.child.id());
                let deadline = Instant::now() + grace;
                let mut status = self.child.try_wait()?;
                while Instant::now() < deadline && self.job.active_processes()? > 0 {
                    if status.is_none() {
                        status = self.child.try_wait()?;
                    }
                    thread::sleep(
                        Duration::from_millis(10)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                if self.job.active_processes()? > 0 {
                    self.job.terminate()?;
                }
                match status {
                    Some(status) => Ok(status),
                    None => self.child.wait(),
                }
            }
            #[cfg(not(windows))]
            {
                let _ = grace;
                let _ = self.child.kill();
                self.child.wait()
            }
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

#[cfg(windows)]
const WINDOWS_CREATE_SUSPENDED: u32 = 0x0000_0004;
#[cfg(windows)]
const WINDOWS_CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const WINDOWS_JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
#[cfg(windows)]
const WINDOWS_JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: i32 = 1;
#[cfg(windows)]
const WINDOWS_JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
#[cfg(windows)]
const WINDOWS_PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const WINDOWS_THREAD_SUSPEND_RESUME: u32 = 0x0002;
#[cfg(windows)]
const WINDOWS_CTRL_BREAK_EVENT: u32 = 1;
#[cfg(windows)]
const WINDOWS_TOOLHELP_SNAPSHOT_THREADS: u32 = 0x0000_0004;

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
impl WindowsFileTime {
    fn as_u64(&self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct WindowsJobBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct WindowsIoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct WindowsJobExtendedLimitInformation {
    basic_limit_information: WindowsJobBasicLimitInformation,
    io_info: WindowsIoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct WindowsJobBasicAccountingInformation {
    total_user_time: i64,
    total_kernel_time: i64,
    this_period_total_user_time: i64,
    this_period_total_kernel_time: i64,
    total_page_fault_count: u32,
    total_processes: u32,
    active_processes: u32,
    total_terminated_processes: u32,
}

#[cfg(windows)]
#[repr(C)]
#[allow(dead_code)]
struct WindowsThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(
        attributes: *mut std::ffi::c_void,
        name: *const u16,
    ) -> *mut std::ffi::c_void;
    fn SetInformationJobObject(
        job: *mut std::ffi::c_void,
        class: i32,
        information: *mut std::ffi::c_void,
        information_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
    fn QueryInformationJobObject(
        job: *mut std::ffi::c_void,
        class: i32,
        information: *mut std::ffi::c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn GenerateConsoleCtrlEvent(event: u32, process_group_id: u32) -> i32;
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut std::ffi::c_void;
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation_time: *mut WindowsFileTime,
        exit_time: *mut WindowsFileTime,
        kernel_time: *mut WindowsFileTime,
        user_time: *mut WindowsFileTime,
    ) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut std::ffi::c_void;
    fn Thread32First(snapshot: *mut std::ffi::c_void, entry: *mut WindowsThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut std::ffi::c_void, entry: *mut WindowsThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit_handle: i32, thread_id: u32) -> *mut std::ffi::c_void;
    fn ResumeThread(thread: *mut std::ffi::c_void) -> u32;
}

#[cfg(windows)]
struct WindowsJob {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsJob {
    fn new() -> io::Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};

        let raw = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut information = WindowsJobExtendedLimitInformation::default();
        information.basic_limit_information.limit_flags =
            WINDOWS_JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let result = unsafe {
            SetInformationJobObject(
                std::os::windows::io::AsRawHandle::as_raw_handle(&handle),
                WINDOWS_JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut information as *mut WindowsJobExtendedLimitInformation).cast(),
                std::mem::size_of::<WindowsJobExtendedLimitInformation>() as u32,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    fn assign_and_resume(&self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;

        let assigned = unsafe {
            AssignProcessToJobObject(
                AsRawHandle::as_raw_handle(&self.handle),
                AsRawHandle::as_raw_handle(child),
            )
        };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        windows_resume_primary_thread(child.id())
    }

    fn active_processes(&self) -> io::Result<u32> {
        use std::os::windows::io::AsRawHandle;

        let mut information = WindowsJobBasicAccountingInformation::default();
        let result = unsafe {
            QueryInformationJobObject(
                AsRawHandle::as_raw_handle(&self.handle),
                WINDOWS_JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                (&mut information as *mut WindowsJobBasicAccountingInformation).cast(),
                std::mem::size_of::<WindowsJobBasicAccountingInformation>() as u32,
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(information.active_processes)
    }

    fn terminate(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;

        let result = unsafe { TerminateJobObject(AsRawHandle::as_raw_handle(&self.handle), 1) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn windows_process_creation_time_from_handle(child: &Child) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    windows_process_creation_time_from_raw_handle(AsRawHandle::as_raw_handle(child))
}

#[cfg(windows)]
fn windows_process_creation_time(process_id: u32) -> io::Result<u64> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    let raw = unsafe { OpenProcess(WINDOWS_PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    windows_process_creation_time_from_raw_handle(std::os::windows::io::AsRawHandle::as_raw_handle(
        &handle,
    ))
}

#[cfg(windows)]
fn windows_process_creation_time_from_raw_handle(
    process: std::os::windows::io::RawHandle,
) -> io::Result<u64> {
    let mut creation = WindowsFileTime::default();
    let mut exit = WindowsFileTime::default();
    let mut kernel = WindowsFileTime::default();
    let mut user = WindowsFileTime::default();
    let result =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(creation.as_u64())
}

#[cfg(windows)]
fn windows_resume_primary_thread(process_id: u32) -> io::Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    let snapshot = unsafe { CreateToolhelp32Snapshot(WINDOWS_TOOLHELP_SNAPSHOT_THREADS, 0) };
    if snapshot as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = WindowsThreadEntry32 {
        size: std::mem::size_of::<WindowsThreadEntry32>() as u32,
        usage: 0,
        thread_id: 0,
        owner_process_id: 0,
        base_priority: 0,
        delta_priority: 0,
        flags: 0,
    };
    let mut found = unsafe {
        Thread32First(
            std::os::windows::io::AsRawHandle::as_raw_handle(&snapshot),
            &mut entry,
        )
    };
    while found != 0 {
        if entry.owner_process_id == process_id {
            let thread = unsafe { OpenThread(WINDOWS_THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { ResumeThread(std::os::windows::io::AsRawHandle::as_raw_handle(&thread)) }
                == u32::MAX
            {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe {
            Thread32Next(
                std::os::windows::io::AsRawHandle::as_raw_handle(&snapshot),
                &mut entry,
            )
        };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not find suspended harness thread",
    ))
}

#[cfg(windows)]
fn windows_generate_console_ctrl_break(process_group_id: u32) -> io::Result<()> {
    if unsafe { GenerateConsoleCtrlEvent(WINDOWS_CTRL_BREAK_EVENT, process_group_id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{ProcessExit, SupervisedProcess};
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    struct TestDirectory(PathBuf);

    #[cfg(unix)]
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

    #[cfg(unix)]
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
