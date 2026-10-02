use clap::Parser;
use std::collections::BTreeMap;
use std::error::Error;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tines_runner_rs::protocol::client::RunLogBuffer;

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
    let mut workers: BTreeMap<String, JoinHandle<Result<(), String>>> = BTreeMap::new();
    let fatal_error = Arc::new(Mutex::new(None::<String>));
    let loop_fatal_error = Arc::clone(&fatal_error);
    let poll_result = poller.run_with(
        |response, state| {
            let fresh_capabilities = response
                .assignments
                .iter()
                .any(|assignment| {
                    assignment.effort.is_some()
                        && !response
                            .released_assignments
                            .contains(&assignment.run.id)
                })
                .then(|| state.refresh_effort_capabilities(true).clone());
            for assignment in &response.assignments {
                if response
                    .released_assignments
                    .contains(&assignment.run.id)
                {
                    tracing::info!(
                        run_id = %assignment.run.id,
                        "assignment was released by Tines and will not be launched"
                    );
                    continue;
                }
                match state.admit_assignment(assignment.run.id.clone()) {
                    tines_runner_rs::poll::AssignmentAdmission::Accepted => {}
                    tines_runner_rs::poll::AssignmentAdmission::AlreadyOwned => {
                        tracing::info!(
                            run_id = %assignment.run.id,
                            "duplicate assignment delivery ignored because the run is already owned"
                        );
                        continue;
                    }
                    tines_runner_rs::poll::AssignmentAdmission::AtCapacity => {
                        tracing::warn!(
                            run_id = %assignment.run.id,
                            concurrency = state.effective_concurrency(),
                            "assignment declined because all local concurrency slots are in use"
                        );
                        state.decline_assignment(assignment.run.id.clone());
                        continue;
                    }
                }
                if assignment.effort.is_some() {
                    let capabilities = fresh_capabilities
                        .as_ref()
                        .expect("effort assignments trigger a fresh capability probe");
                    if let Some(reason) =
                        tines_runner_rs::effort::assignment_effort_rejection(assignment, capabilities)
                    {
                        tracing::warn!(
                            run_id = %assignment.run.id,
                            model = assignment.run.model.as_deref().unwrap_or("provider default"),
                            error = %reason,
                            "assignment declined because Codex cannot verify the requested effort"
                        );
                        state.decline_assignment(assignment.run.id.clone());
                        continue;
                    }
                }
                match tines_runner_rs::assignment::resolve_assignment(
                    &config,
                    &issue_client,
                    assignment,
                ) {
                    Ok(resolved) => {
                        let mut run_logs = RunLogBuffer::new();
                        let workspace = match tines_runner_rs::workspace::MaterializedWorkspace::create_with_git_log(
                            &resolved.resolution().config.workspace_parent,
                            resolved.assignment(),
                            &config.server_url,
                            |chunk| {
                                tracing::info!(
                                    run_id = %assignment.run.id,
                                    git_output = %chunk.trim_end(),
                                    "repository checkout progress"
                                );
                                run_logs.buffer_preparation_output(chunk);
                            },
                        ) {
                            Ok(workspace) => workspace,
                            Err(error) => {
                                tracing::error!(
                                    run_id = %assignment.run.id,
                                    error = %error,
                                    "assignment failed during workspace materialization"
                                );
                                let output = run_logs.preparation_output();
                                let failure = if output.is_empty() {
                                    error.to_string()
                                } else {
                                    format!("{error}\nRepository checkout output:\n{output}")
                                };
                                state.fail_assignment(assignment.run.id.clone(), failure);
                                continue;
                            }
                        };
                        tracing::info!(
                            run_id = %assignment.run.id,
                            project = resolved.context().project(),
                            workflow = resolved.context().workflow(),
                            state = resolved.context().state(),
                            matched_overrides = ?resolved.resolution().matching_overrides(),
                            workspace = %workspace.path().display(),
                            "assignment workspace materialized and queued"
                        );
                        let run_id = assignment.run.id.clone();
                        state.queue_assignment(
                            tines_runner_rs::assignment::PreparedAssignment::new(
                                resolved, workspace,
                            )
                            .with_run_log_buffer(run_logs),
                        );
                        let Some(prepared) = state.take_assignment(&run_id) else {
                            continue;
                        };
                        let capabilities = state.refresh_effort_capabilities(false).clone();
                        let worker_capabilities = capabilities.clone();
                        let worker_assignment = prepared.clone();
                        let worker_connection = execution_connection.clone();
                        let worker_client = issue_client.clone();
                        match thread::Builder::new()
                            .name(format!("runner-run-{}", run_id))
                            .spawn(move || {
                                tines_runner_rs::execution::execute_assignment(
                                    worker_assignment,
                                    &worker_connection,
                                    &worker_client,
                                    &worker_capabilities,
                                )
                                .map_err(|error| error.to_string())
                            })
                        {
                            Ok(worker) => {
                                workers.insert(run_id, worker);
                            }
                            Err(error) => {
                                tracing::error!(
                                    run_id,
                                    error = %error,
                                    "could not start assignment executor thread; executing on poll thread"
                                );
                                match tines_runner_rs::execution::execute_assignment(
                                    prepared,
                                    &execution_connection,
                                    &issue_client,
                                    &capabilities,
                                ) {
                                    Ok(()) => state.release_run(&run_id),
                                    Err(error) => {
                                        *fatal_error
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                            Some(error.to_string());
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        state.decline_assignment(assignment.run.id.clone());
                        tracing::error!(
                            run_id = %assignment.run.id,
                            error = %error,
                            "assignment declined because required configuration metadata is unavailable"
                        );
                    }
                }
            }
            for run_id in &response.cancels {
                tracing::warn!(
                    run_id,
                    "supervisor settled run; executor must stop it without finish reporting"
                );
            }
            for request in &response.cancel_requests {
                tracing::warn!(run_id = %request.run_id, "supervisor requested run cancellation");
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
                .filter(|(_, worker)| worker.is_finished())
                .map(|(run_id, _)| run_id.clone())
                .collect::<Vec<_>>();
            for run_id in completed {
                let Some(worker) = workers.remove(&run_id) else {
                    continue;
                };
                match worker.join() {
                    Ok(Ok(())) => state.release_run(&run_id),
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
        match worker.join() {
            Ok(Ok(())) => {}
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
