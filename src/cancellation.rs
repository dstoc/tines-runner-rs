//! Shared cancellation state for one locally owned assignment.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A cloneable signal shared by the poll loop and one assignment worker.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Ask the assignment worker to stop local work.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Wait for the duration unless cancellation arrives first.
    pub fn wait(&self, duration: Duration) {
        let deadline = std::time::Instant::now() + duration;
        while !self.is_cancelled() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }
}
