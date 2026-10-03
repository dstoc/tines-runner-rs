//! Process-wide SIGINT/SIGTERM handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag shared by the poll loop and assignment workers.
#[derive(Clone, Debug)]
pub struct ShutdownSignal(Arc<AtomicBool>);

impl ShutdownSignal {
    /// Create a signal flag without installing process handlers.
    pub fn inactive() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Install one handler for Ctrl-C and SIGTERM.
    pub fn install() -> Result<Self, ctrlc::Error> {
        let requested = Arc::new(AtomicBool::new(false));
        let handler_flag = Arc::clone(&requested);
        ctrlc::set_handler(move || {
            handler_flag.store(true, Ordering::SeqCst);
        })?;
        Ok(Self(requested))
    }

    /// Return whether the process received a shutdown signal.
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Read the signal flag while supervising a child process.
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
}
