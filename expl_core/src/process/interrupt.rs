//! [`Interrupt`] — a cooperative stop flag, raised by SIGTERM/SIGINT or by
//! a stop request recorded for the process.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared stop flag. Clones observe the same flag.
#[derive(Debug, Clone, Default)]
pub struct Interrupt(Arc<AtomicBool>);

impl Interrupt {
    /// A flag that is raised on SIGTERM or SIGINT.
    ///
    /// # Errors
    ///
    /// I/O error when the signal handlers cannot be installed.
    pub fn on_signals() -> std::io::Result<Self> {
        let interrupt = Self::default();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            signal_hook::flag::register(signal, Arc::clone(&interrupt.0))?;
        }
        Ok(interrupt)
    }

    /// Raise the flag.
    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the flag is raised.
    #[must_use]
    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
