//! Building blocks for the independent Tines runner.

pub mod assignment;
pub mod assignment_worker;
pub mod cancellation;
pub mod config;
pub mod credentials;
pub mod diagnostic;
pub mod effort;
pub mod execution;
pub mod execution_protocol;
pub mod executor;
pub mod executor_capabilities;
pub mod executor_events;
pub mod executor_transport;
pub mod logging;
pub mod poll;
pub mod process;
pub mod protocol;
pub mod recovery;
pub mod retention;
pub mod runner;
pub mod shutdown;

/// Version of this runner, set from the Cargo package metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
