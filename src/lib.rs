//! Building blocks for the independent Tines runner.

pub mod assignment;
pub mod assignment_worker;
pub mod cancellation;
pub mod codex;
pub mod codex_adapter;
pub mod codex_stream;
pub mod config;
pub mod credentials;
pub mod effort;
pub mod execution;
pub mod execution_protocol;
pub mod executor;
pub mod executor_capabilities;
pub mod executor_transport;
pub mod finish;
pub mod harness;
pub mod logging;
pub mod poll;
pub mod process;
pub mod protocol;
pub mod recovery;
pub mod retention;
pub mod runner;
pub mod shutdown;
pub mod workspace;

/// Version of this runner, set from the Cargo package metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
