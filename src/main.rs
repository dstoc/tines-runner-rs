use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "tines-runner-rs",
    version,
    about = "An independent Rust runner for Tines assignments"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    tines_runner_rs::logging::init();
    tracing::info!(version = tines_runner_rs::VERSION, "runner initialized");
}
