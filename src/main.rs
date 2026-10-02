use clap::Parser;
use std::collections::BTreeMap;
use std::error::Error;
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
    /// Validate configuration and stored credentials, then exit without polling.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tines_runner_rs::logging::init();

    match start_runner(cli.check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "runner startup failed");
            ExitCode::FAILURE
        }
    }
}

fn start_runner(check: bool) -> Result<(), Box<dyn Error>> {
    let config = tines_runner_rs::config::Config::load_default()?;
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

    let issue_client = tines_runner_rs::protocol::client::Client::new(config.server_url.as_str())?;
    let execution_connection = connection.clone();
    let mut poller = tines_runner_rs::poll::PollLoop::new(connection, &config);
    let boot_id = poller.state().instance_id().to_owned();
    tracing::info!(instance_id = %boot_id, "runner poll loop started");
    let mut workers: BTreeMap<String, AssignmentWorker> = BTreeMap::new();
    let fatal_error = Arc::new(Mutex::new(None::<String>));
    let loop_fatal_error = Arc::clone(&fatal_error);
    let poll_result = poller.run_with(
        |response, state| {
            let mut canceled_ids = std::collections::BTreeSet::new();
            for request in &response.cancel_requests {
                canceled_ids.insert(request.run_id.clone());
                if let Some(worker) = workers.get_mut(&request.run_id) {
                    worker.cancellation_ack = Some(request.token.clone());
                    worker.cancellation.cancel();
                } else {
                    state.acknowledge_cancellation(request.run_id.clone(), request.token.clone());
                }
                tracing::warn!(run_id = %request.run_id, "supervisor requested run cancellation");
            }
            for run_id in &response.cancels {
                canceled_ids.insert(run_id.clone());
                if let Some(worker) = workers.get(run_id) {
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
                let worker_cancellation = cancellation.clone();
                let worker_config = config.clone();
                let worker_connection = execution_connection.clone();
                let worker_client = issue_client.clone();
                let worker_capabilities = capabilities.clone();
                let worker_assignment = assignment.clone();
                match thread::Builder::new()
                    .name(format!("runner-run-{run_id}"))
                    .spawn(move || {
                        tines_runner_rs::assignment_worker::run_assignment(
                            &worker_config,
                            &worker_connection,
                            &worker_client,
                            worker_assignment,
                            &worker_capabilities,
                            &worker_cancellation,
                        )
                    })
                {
                    Ok(handle) => {
                        workers.insert(
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

            let completed = workers
                .iter()
                .filter(|(_, worker)| worker.handle.is_finished())
                .map(|(run_id, _)| run_id.clone())
                .collect::<Vec<_>>();
            for run_id in completed {
                let Some(worker) = workers.remove(&run_id) else {
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
                                tracing::error!(run_id, error, "assignment declined because required metadata or Codex capability is unavailable");
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
        },
        || {
            loop_fatal_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        },
        std::thread::sleep,
    );

    for (run_id, worker) in workers {
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
