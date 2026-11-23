//! Worker lifetime and the lease shared between a reaction and its heartbeat.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::Interrupt;

/// A worker's one in-flight delivery. The lease may be any application type.
#[derive(Debug)]
pub struct Active<L>(Mutex<Option<(L, Interrupt)>>);

impl<L> Default for Active<L> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<L> Active<L> {
    /// Run one reaction, exposing its lease to the heartbeat until it returns.
    pub fn react<T>(&self, lease: L, interrupt: &Interrupt, run: impl FnOnce() -> T) -> T {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((lease, interrupt.clone()));
        let _clear = Clear(self);
        run()
    }

    /// Renew the current lease. Return false from `renew` to cancel the
    /// reaction because a different worker now holds the delivery.
    pub fn renew(&self, renew: impl FnOnce(&L) -> bool) {
        let held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((lease, interrupt)) = held.as_ref()
            && !renew(lease)
        {
            interrupt.lose_lease();
        }
    }
}

struct Clear<'a, L>(&'a Active<L>);

impl<L> Drop for Clear<'_, L> {
    fn drop(&mut self) {
        self.0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// Run a worker alongside its heartbeat. The application supplies its own
/// durable queue, event selection and settlement logic.
pub fn with_heartbeat<L: Send, T>(
    interval: Duration,
    mut beat: impl FnMut(&Active<L>) + Send,
    serve: impl FnOnce(&Active<L>) -> T,
) -> T {
    let active = Active::default();
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        let active = &active;
        let finished = &finished;
        let heartbeat = scope.spawn(move || {
            let mut next = Instant::now();
            while !finished.load(Ordering::Acquire) {
                if Instant::now() >= next {
                    beat(active);
                    next = Instant::now() + interval;
                }
                thread::park_timeout(next.saturating_duration_since(Instant::now()));
            }
        });
        let _stop = Stop {
            finished,
            heartbeat: heartbeat.thread(),
        };
        serve(active)
    })
}

struct Stop<'a> {
    finished: &'a AtomicBool,
    heartbeat: &'a thread::Thread,
}

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        self.heartbeat.unpark();
    }
}
