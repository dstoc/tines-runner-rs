use clap::Parser;
use std::cell::RefCell;
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
    /// Load configuration from this file instead of the platform default.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Validate configuration and stored credentials, then exit without polling.
    #[arg(long)]
    check: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, clap::Subcommand)]
enum CliCommand {
    /// Read and validate one execution request from stdin.
    Execute,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tines_runner_rs::logging::init();

    if let Some(CliCommand::Execute) = cli.command {
        return execute_stdin();
    }

    match start_runner(cli.check, cli.config.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "runner startup failed");
            ExitCode::FAILURE
        }
    }
}

fn execute_stdin() -> ExitCode {
    match tines_runner_rs::execution_protocol::read_execution_request(std::io::stdin().lock()) {
        Ok(request) => {
            let _adapter = match tines_runner_rs::harness::adapter_for(&request.execution.harness) {
                Ok(adapter) => adapter,
                Err(error) => {
                    eprintln!("executor request rejected: {error}");
                    return ExitCode::FAILURE;
                }
            };
            match tines_runner_rs::executor::prepare_workspace(&request, |_| {}) {
                Ok(workspace) => match workspace.cleanup() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => report_preparation_failure(&request, &error),
                },
                Err(error) => report_preparation_failure(&request, &error),
            }
        }
        Err(error) => {
            eprintln!("executor request rejected: {error}");
            ExitCode::FAILURE
        }
    }
}

fn report_preparation_failure(
    request: &tines_runner_rs::execution_protocol::ExecutionRequest,
    error: &dyn std::fmt::Display,
) -> ExitCode {
    match tines_runner_rs::executor::render_preparation_failure(request, error) {
        Ok(event) => {
            match std::io::Write::write_all(&mut std::io::stdout().lock(), event.as_bytes()) {
                Ok(()) => ExitCode::FAILURE,
                Err(_) => {
                    eprintln!("executor could not write its failure event");
                    ExitCode::FAILURE
                }
            }
        }
        Err(_) => {
            eprintln!("executor could not encode its failure event");
            ExitCode::FAILURE
        }
    }
}

fn start_runner(check: bool, config_path: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let config = match config_path {
        Some(path) => tines_runner_rs::config::Config::load(path)?,
        None => tines_runner_rs::config::Config::load_default()?,
    };
    let shutdown = if check {
        None
    } else {
        Some(tines_runner_rs::shutdown::ShutdownSignal::install()?)
    };
    let connection = tines_runner_rs::runner::RunnerConnection::connect(&config)?;
    if check {
        connection.verify()?;
    }

    tracing::info!(
        version = tines_runner_rs::VERSION,
        runner_id = connection.credentials().runner_id(),
        registered = connection.registered(),
        "runner credentials ready"
    );

    if check {
        return Ok(());
    }

    tines_runner_rs::retention::prune_retained_roots(
        config.workspace_parents(),
        &config.workspace_retention,
    )?;
    let shutdown = shutdown.ok_or_else(|| {
        std::io::Error::other("daemon shutdown handler was not installed before polling")
    })?;
    let active_runs = tines_runner_rs::recovery::ActiveRunStore::open(
        config.credentials_file.with_file_name("active-runs.json"),
    )?;
    let recovered_runs = tines_runner_rs::recovery::recover_active_runs(
        &active_runs,
        &config.workspace_retention,
        &config.workspace_parents(),
    )?;
    for run_id in recovered_runs {
        tracing::info!(run_id, "recovered interrupted assignment before polling");
    }
    let issue_client = tines_runner_rs::protocol::client::Client::new(config.server_url.as_str())?;
    let execution_connection = connection.clone();
    let mut poller = tines_runner_rs::poll::PollLoop::new(connection, &config);
    let boot_id = poller.state().instance_id().to_owned();
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
                tracing::warn!(run_id = %request.run_id, "supervisor requested run cancellation");
            }
            for run_id in &response.cancels {
                canceled_ids.insert(run_id.clone());
                if let Some(worker) = workers.borrow().get(run_id) {
                    worker.cancellation.cancel();
                }
                tracing::warn!(run_id, "supervisor settled run; stopping local work without finish reporting");
            }

            let capabilities = state.refresh_effort_capabilities(false).clone();
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
                    tracing::info!(run_id, "duplicate assignment delivery ignored because the run is already owned");
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
                        tracing::info!(run_id, "duplicate assignment delivery ignored because the run is already owned");
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
                let worker_capabilities = capabilities.clone();
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
                        tines_runner_rs::assignment_worker::run_assignment(
                            &worker_config,
                            &worker_connection,
                            &worker_client,
                            worker_assignment,
                            run_logs,
                            &worker_capabilities,
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
            if let Some(control) = &response.concurrency_control {
                tracing::info!(
                    concurrency = state.effective_concurrency(),
                    available = control.available,
                    "runner concurrency policy updated"
                );
            }

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
                            "assignment declined because required metadata or Codex capability is unavailable"
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
