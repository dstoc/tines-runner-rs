use clap::{CommandFactory, Parser};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tines_runner_rs::assignment_worker::AssignmentTaskOutcome;
use tines_runner_rs::cancellation::CancellationToken;

struct AssignmentWorker {
    cancellation: CancellationToken,
    cancellation_ack: Option<String>,
    handle: JoinHandle<Result<AssignmentTaskOutcome, String>>,
}

#[derive(Debug, Parser)]
#[command(
    name = "tines-runner-rs",
    version,
    about = "An independent Rust runner for Tines assignments"
)]
struct Cli {
    /// Load runner configuration from this file (required for daemon, --check, and register).
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Select one named runner definition from the config file.
    #[arg(long, value_name = "ID", global = true)]
    runner: Option<String>,

    /// Validate configuration and stored credentials, then exit without polling.
    #[arg(long)]
    check: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, clap::Subcommand)]
enum CliCommand {
    /// Register the configured runner and write credentials to an explicit file.
    Register {
        /// Write the returned runner ID and token to this file.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
    },
    /// Read and validate one execution request from stdin.
    Execute,
    /// Report harness and effort capabilities from this executor environment.
    Capabilities,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tines_runner_rs::logging::init();

    let config_required = !matches!(
        cli.command.as_ref(),
        Some(CliCommand::Execute | CliCommand::Capabilities)
    );
    if config_required && cli.config.is_none() {
        let message = if matches!(cli.command.as_ref(), Some(CliCommand::Register { .. })) {
            "--config <PATH> is required for register"
        } else {
            "--config <PATH> is required for daemon operation and --check"
        };
        Cli::command()
            .error(clap::error::ErrorKind::MissingRequiredArgument, message)
            .exit();
    }
    if cli.runner.is_some() && cli.config.is_none() {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--config <PATH> is required when selecting --runner <ID>",
            )
            .exit();
    }

    match cli.command {
        Some(CliCommand::Register { output }) => {
            return match register_runner(
                cli.config.as_deref().expect("register requires --config"),
                cli.runner.as_deref(),
                &output,
            ) {
                Ok(credentials) => {
                    tracing::info!(
                        runner_id = credentials.runner_id(),
                        output = %output.display(),
                        "runner credentials registered and saved"
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    tracing::error!(error = %error, "runner registration failed");
                    ExitCode::FAILURE
                }
            };
        }
        Some(CliCommand::Execute) => return execute_stdin(),
        Some(CliCommand::Capabilities) => {
            return match tines_runner_rs::executor_capabilities::discover_for_cli(
                tines_runner_rs::VERSION,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("executor could not write its capability document: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }

    match start_runner(
        cli.check,
        cli.config
            .as_deref()
            .expect("runner mode requires --config"),
        cli.runner.as_deref(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "runner startup failed");
            ExitCode::FAILURE
        }
    }
}

fn register_runner(
    config_path: &Path,
    runner_id: Option<&str>,
    output_path: &Path,
) -> Result<tines_runner_rs::credentials::RunnerCredentials, Box<dyn Error>> {
    let config = tines_runner_rs::config::Config::load_for_runner(config_path, runner_id)?;
    Ok(tines_runner_rs::runner::RunnerConnection::register(
        &config,
        output_path,
    )?)
}

fn execute_stdin() -> ExitCode {
    let shutdown = match tines_runner_rs::shutdown::ShutdownSignal::install() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            eprintln!("executor could not install shutdown handler: {error}");
            return ExitCode::FAILURE;
        }
    };
    match tines_runner_rs::execution_protocol::read_execution_request(std::io::stdin().lock()) {
        Ok(request) => tines_runner_rs::executor::execute_request(
            &request,
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
            &shutdown,
        ),
        Err(error) => {
            eprintln!("executor request rejected: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start_runner(
    check: bool,
    config_path: &Path,
    runner_id: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let config = tines_runner_rs::config::Config::load_for_runner(config_path, runner_id)?;
    let shutdown = if check {
        None
    } else {
        Some(tines_runner_rs::shutdown::ShutdownSignal::install()?)
    };
    let connection = tines_runner_rs::runner::RunnerConnection::connect(&config)?;
    let _ownership = if check {
        None
    } else {
        Some(tines_runner_rs::ownership::DaemonOwnershipGuard::acquire(
            &config.server_url,
            connection.credentials(),
        )?)
    };
    if check {
        connection.verify()?;
    }

    tracing::info!(
        version = tines_runner_rs::VERSION,
        local_id = %config.local_id,
        runner_id = connection.credentials().runner_id(),
        registered = connection.registered(),
        "runner credentials ready"
    );

    if check {
        return Ok(());
    }

    let shutdown = shutdown.ok_or_else(|| {
        std::io::Error::other("daemon shutdown handler was not installed before polling")
    })?;
    let active_runs = tines_runner_rs::recovery::ActiveRunStore::open(config.active_runs_file())?;
    let recovered_runs = tines_runner_rs::recovery::recover_active_runs(
        &active_runs,
        &config.workspace_retention,
        &config.legacy_workspace_roots(),
    )?;
    for run_id in recovered_runs {
        tracing::info!(run_id, "recovered interrupted assignment before polling");
    }
    let issue_client = tines_runner_rs::protocol::client::Client::new(config.server_url.as_str())?;
    let execution_connection = connection.clone();
    let default_capabilities_transport =
        tines_runner_rs::executor_transport::ExecutorTransport::for_capabilities(&config);
    let mut poller = tines_runner_rs::poll::PollLoop::new(connection, &config);
    let boot_id = poller.state().instance_id().to_owned();
    let failure_reporter = poller.state().failure_reporter();
    let last_logged_concurrency = Cell::new(config.max_concurrent as u32);
    tracing::info!(instance_id = %boot_id, "runner poll loop started");
    let workers = RefCell::new(BTreeMap::<String, AssignmentWorker>::new());
    let fatal_error = Arc::new(Mutex::new(None::<String>));
    let loop_fatal_error = Arc::clone(&fatal_error);
    let poll_shutdown = shutdown.clone();
    let poll_result = poller.run_controlled(
        |state| {
            state.set_draining(shutdown.is_requested());
            reap_completed_workers(&workers, state, &fatal_error);
        },
        |response, state| {
            let mut canceled_ids = std::collections::BTreeSet::new();
            for request in &response.cancel_requests {
                canceled_ids.insert(request.run_id.clone());
                if let Some(worker) = workers.borrow_mut().get_mut(&request.run_id) {
                    worker.cancellation_ack = Some(request.token.clone());
                    worker.cancellation.cancel();
                } else {
                    state.acknowledge_cancellation(request.run_id.clone(), request.token.clone());
                }
                tracing::info!(run_id = %request.run_id, "supervisor requested run cancellation");
            }
            for run_id in &response.cancels {
                canceled_ids.insert(run_id.clone());
                if let Some(worker) = workers.borrow().get(run_id) {
                    worker.cancellation.cancel();
                }
                tracing::info!(run_id, "supervisor settled run; stopping local work without finish reporting");
            }

            let force_capability_refresh = response
                .assignments
                .iter()
                .any(|assignment| assignment.effort.is_some());
            let capabilities = state
                .refresh_executor_capabilities(force_capability_refresh)
                .clone();
            for assignment in &response.assignments {
                let run_id = assignment.run.id.clone();
                if response.released_assignments.contains(&run_id) {
                    tracing::info!(run_id, "assignment was released by Tines and will not be launched");
                    continue;
                }
                if canceled_ids.contains(&run_id) {
                    continue;
                }
                if state.owns_run(&run_id) {
                    tracing::debug!(run_id, "duplicate assignment delivery ignored because the run is already owned");
                    continue;
                }
                if shutdown.is_requested() {
                    state.decline_assignment(run_id.clone());
                    tracing::info!(run_id, "assignment declined because the runner is draining");
                    continue;
                }
                match state.admit_assignment(run_id.clone()) {
                    tines_runner_rs::poll::AssignmentAdmission::Accepted => {}
                    tines_runner_rs::poll::AssignmentAdmission::AlreadyOwned => {
                        tracing::debug!(run_id, "duplicate assignment delivery ignored because the run is already owned");
                        continue;
                    }
                    tines_runner_rs::poll::AssignmentAdmission::AtCapacity => {
                        tracing::warn!(run_id, concurrency = state.effective_concurrency(), "assignment declined because all local concurrency slots are in use");
                        state.decline_assignment(run_id);
                        continue;
                    }
                }

                let cancellation = CancellationToken::default();
                let run_logs =
                    tines_runner_rs::protocol::client::RunLogBuffer::for_assignment(assignment);
                state.own_run_with_logs(run_id.clone(), run_logs.clone());
                let worker_cancellation = cancellation.clone();
                let worker_config = config.clone();
                let worker_connection = execution_connection.clone();
                let worker_client = issue_client.clone();
                let worker_default_capabilities_transport =
                    default_capabilities_transport.clone();
                let worker_capabilities = capabilities.clone();
                let worker_failure_reporter = failure_reporter.clone();
                let worker_assignment = assignment.clone();
                let worker_shutdown = shutdown.clone();
                let worker_active_runs = active_runs.clone();
                match thread::Builder::new()
                    .name(format!("runner-run-{run_id}"))
                    .spawn(move || {
                        let context = tines_runner_rs::execution::ExecutionContext::new(
                            &worker_shutdown,
                            &worker_active_runs,
                        );
                        tines_runner_rs::assignment_worker::run_assignment_with_reporter(
                            &worker_config,
                            &worker_connection,
                            &worker_client,
                            worker_assignment,
                            run_logs,
                            &worker_default_capabilities_transport,
                            &worker_capabilities,
                            &worker_failure_reporter,
                            &worker_cancellation,
                            &context,
                        )
                    })
                {
                    Ok(handle) => {
                        workers.borrow_mut().insert(
                            run_id,
                            AssignmentWorker {
                                cancellation,
                                cancellation_ack: None,
                                handle,
                            },
                        );
                    }
                    Err(error) => {
                        tracing::error!(run_id, error = %error, "could not start assignment worker; requesting safe release");
                        state.release_run(&run_id);
                        state.decline_assignment(run_id);
                    }
                }
            }
            let concurrency = state.effective_concurrency();
            let previous = last_logged_concurrency.replace(concurrency);
            tines_runner_rs::logging::log_effective_concurrency_change(previous, concurrency);

            reap_completed_workers(&workers, state, &fatal_error);
        },
        |state| {
            if loop_fatal_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            {
                return false;
            }
            if poll_shutdown.is_requested() {
                return state.active_run_count() > 0
                    || state.has_pending_declines()
                    || state.has_pending_cancellation_acks()
                    || !state.draining_poll_reported();
            }
            true
        },
        std::thread::sleep,
    );

    for (run_id, worker) in workers.into_inner() {
        match worker.handle.join() {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::error!(run_id, error, "assignment execution did not settle");
                *fatal_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
            }
            Err(_) => {
                let error = "assignment executor thread panicked".to_owned();
                tracing::error!(run_id, error, "assignment execution did not settle");
                *fatal_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
            }
        }
    }
    poll_result?;
    if let Some(error) = fatal_error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return Err(std::io::Error::other(error).into());
    }

    Ok(())
}

fn reap_completed_workers(
    workers: &RefCell<BTreeMap<String, AssignmentWorker>>,
    state: &mut tines_runner_rs::poll::PollState,
    fatal_error: &Arc<Mutex<Option<String>>>,
) {
    let completed = workers
        .borrow()
        .iter()
        .filter(|(_, worker)| worker.handle.is_finished())
        .map(|(run_id, _)| run_id.clone())
        .collect::<Vec<_>>();
    for run_id in completed {
        let Some(worker) = workers.borrow_mut().remove(&run_id) else {
            continue;
        };
        match worker.handle.join() {
            Ok(Ok(outcome)) => {
                if let Some(token) = worker.cancellation_ack {
                    state.acknowledge_cancellation(run_id.clone(), token);
                }
                match outcome {
                    AssignmentTaskOutcome::Declined(error)
                        if !worker.cancellation.is_cancelled() =>
                    {
                        tracing::error!(
                            run_id,
                            error,
                            "assignment declined because required metadata or launch capability is unavailable"
                        );
                        state.decline_assignment(run_id);
                    }
                    _ => state.release_run(&run_id),
                }
            }
            Ok(Err(error)) => {
                tracing::error!(run_id, error, "assignment execution did not settle");
                *fatal_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
            }
            Err(_) => {
                let error = "assignment executor thread panicked".to_owned();
                tracing::error!(run_id, error, "assignment execution did not settle");
                *fatal_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
            }
        }
    }
}
