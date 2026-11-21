//! [`Interrupt`] — a cooperative stop flag, raised by SIGTERM/SIGINT or by
//! a stop request recorded for the process, and — for one reaction — by the
//! loss of its lease.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared stop flag. Clones observe the same flags.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    stop: Arc<AtomicBool>,
    /// Set only on the interrupt of one reaction (see [`Self::for_reaction`]).
    lease_lost: Option<Arc<AtomicBool>>,
}

impl Interrupt {
    /// A flag that is raised on SIGTERM or SIGINT.
    ///
    /// # Errors
    ///
    /// I/O error when the signal handlers cannot be installed.
    pub fn on_signals() -> std::io::Result<Self> {
        let interrupt = Self::default();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            signal_hook::flag::register(signal, Arc::clone(&interrupt.stop))?;
        }
        Ok(interrupt)
    }

    /// The interrupt of one reaction: raised whenever this one is, and also
    /// when [the lease is lost](Self::lose_lease), which leaves this one
    /// (the process) alone.
    #[must_use]
    pub fn for_reaction(&self) -> Self {
        Self {
            stop: Arc::clone(&self.stop),
            lease_lost: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// Raise the flag: stop the process.
    pub fn raise(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// The reaction's lease was lost: interrupt the reaction only. No effect
    /// on an interrupt not made [for a reaction](Self::for_reaction).
    pub fn lose_lease(&self) {
        if let Some(lost) = &self.lease_lost {
            lost.store(true, Ordering::SeqCst);
        }
    }

    /// Whether the flag is raised, for either reason.
    #[must_use]
    pub fn is_raised(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.lease_lost()
    }

    /// Whether the reaction's lease was lost.
    #[must_use]
    pub fn lease_lost(&self) -> bool {
        self.lease_lost
            .as_ref()
            .is_some_and(|lost| lost.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::Interrupt;

    #[test]
    fn a_lost_lease_interrupts_only_its_reaction() {
        let process = Interrupt::default();
        let reaction = process.for_reaction();
        reaction.lose_lease();
        assert!(reaction.is_raised() && reaction.lease_lost());
        assert!(!process.is_raised());

        let next = process.for_reaction();
        assert!(!next.is_raised());
        process.raise();
        assert!(next.is_raised() && !next.lease_lost());
    }
}
