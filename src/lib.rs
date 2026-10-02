//! Building blocks for the independent Tines runner.

pub mod assignment;
pub mod codex;
pub mod codex_stream;
pub mod config;
pub mod credentials;
pub mod logging;
pub mod poll;
pub mod process;
pub mod protocol;
pub mod runner;
pub mod workspace;

/// Version of this runner, set from the Cargo package metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
