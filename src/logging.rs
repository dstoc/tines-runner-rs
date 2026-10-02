//! Structured diagnostics for the long-running runner process.

use tracing_subscriber::EnvFilter;

/// Initialize JSON tracing output with a filter from `RUST_LOG` or `info`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
}
