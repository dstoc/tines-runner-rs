use clap::Parser;
use std::error::Error;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "tines-runner-rs",
    version,
    about = "An independent Rust runner for Tines assignments"
)]
struct Cli {}

fn main() -> ExitCode {
    let _cli = Cli::parse();
    tines_runner_rs::logging::init();

    match start_runner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "runner startup failed");
            ExitCode::FAILURE
        }
    }
}

fn start_runner() -> Result<(), Box<dyn Error>> {
    let config = tines_runner_rs::config::Config::load_default()?;
    let connection = tines_runner_rs::runner::RunnerConnection::connect(&config)?;
    connection.verify()?;

    tracing::info!(
        version = tines_runner_rs::VERSION,
        runner_id = connection.credentials().runner_id(),
        registered = connection.registered(),
        "runner credentials ready"
    );

    Ok(())
}
