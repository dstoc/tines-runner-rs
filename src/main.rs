use clap::Parser;
use std::error::Error;
use std::process::ExitCode;

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
    let mut poller = tines_runner_rs::poll::PollLoop::new(connection, &config);
    let boot_id = poller.state().instance_id().to_owned();
    tracing::info!(instance_id = %boot_id, "runner poll loop started");
    poller.run_with(
        |response, state| {
            for assignment in &response.assignments {
                match tines_runner_rs::assignment::resolve_assignment(
                    &config,
                    &issue_client,
                    assignment,
                ) {
                    Ok(resolved) => {
                        tracing::info!(
                            run_id = %assignment.run.id,
                            project = resolved.context().project(),
                            workflow = resolved.context().workflow(),
                            state = resolved.context().state(),
                            matched_overrides = ?resolved.resolution().matching_overrides(),
                            "assignment configuration resolved and queued"
                        );
                        state.queue_assignment(resolved);
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
        },
        || true,
        std::thread::sleep,
    )?;

    Ok(())
}
