//! Reaction slots shared by every worker under a concurrency limit.
//!
//! A limit is a directory of slot files, one per allowed reaction; a
//! reaction runs only while its worker holds an exclusive lock on a slot
//! of every limit that applies to its consumer: the consumer's own, and
//! that of each router it is under. The locks are released when the worker
//! drops them or exits, however it exits.

use std::fmt::Write;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use crate::registration::Route;

/// The limits that apply to one consumer's reactions.
#[derive(Debug)]
pub struct Capacity(Vec<(PathBuf, NonZeroUsize)>);

/// One held slot of a limit. Dropping it frees the slot.
#[derive(Debug)]
pub struct Slot(File);

impl Drop for Slot {
    fn drop(&mut self) {
        // An unrelated fork may temporarily have a copy of the file
        // descriptor until it execs. Release the lock here, before close.
        drop(self.0.unlock());
    }
}

impl Capacity {
    /// The limits on `consumer` under `state`, the explorer's state
    /// directory: its own `concurrency`, and each router of `route` it is
    /// under.
    ///
    /// # Errors
    ///
    /// I/O failure creating a slot directory.
    pub fn new(
        state: &Path,
        consumer: &str,
        concurrency: Option<NonZeroUsize>,
        route: Option<&Route<'_>>,
    ) -> io::Result<Self> {
        let root = state.join("slots");
        let mut limits = Vec::new();
        if let Some(limit) = concurrency {
            limits.push((directory(&root, "agent", consumer), limit));
        }
        if let Some(route) = route {
            limits.extend(
                route
                    .groups(consumer)
                    .map(|(name, limit)| (directory(&root, "router", name), limit)),
            );
        }
        for (directory, _) in &limits {
            std::fs::create_dir_all(directory)?;
        }
        Ok(Self(limits))
    }

    /// Take one slot per limit without waiting; `None` when a limit is
    /// full. Dropping the slots frees them all, including on failure or
    /// process exit. Event leases can expire independently without
    /// permitting two live reactions past a limit.
    ///
    /// # Errors
    ///
    /// I/O failure opening or locking a slot file.
    pub fn try_acquire(&self) -> io::Result<Option<Vec<Slot>>> {
        let mut held = Vec::with_capacity(self.0.len());
        for (directory, limit) in &self.0 {
            let Some(file) = try_slot(directory, limit.get())? else {
                return Ok(None);
            };
            held.push(file);
        }
        Ok(Some(held))
    }
}

fn try_slot(directory: &Path, limit: usize) -> io::Result<Option<Slot>> {
    for slot in 0..limit {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(slot.to_string()))?;
        match file.try_lock() {
            Ok(()) => return Ok(Some(Slot(file))),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(None)
}

fn directory(root: &Path, kind: &str, name: &str) -> PathBuf {
    let mut path = root.join(kind);
    // Encode names so slashes, case-insensitive filesystems and long
    // names do not make unrelated groups share a slot directory.
    for chunk in name.as_bytes().chunks(100) {
        let mut component = String::with_capacity(chunk.len() * 2);
        for byte in chunk {
            let _ = write!(component, "{byte:02x}");
        }
        path.push(component);
    }
    path.join("slots")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::registration::{
        ConsumerRegistration, Registration, RouterDeclaration, RoutingPolicy,
    };

    fn limit(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("limit")
    }

    #[test]
    fn workers_need_room_at_each_level_and_release_slots_on_drop() {
        let state = tempfile::tempdir().expect("state");
        let mut registration = Registration::default();
        for (name, group, concurrency) in [
            ("explorer", "exploration", Some(1)),
            ("fringe", "exploration", None),
            ("planner", "planning", Some(1)),
            ("verifier", "verification", Some(2)),
        ] {
            registration.consumer.insert(
                name.to_owned(),
                ConsumerRegistration {
                    on: vec!["goal.added".to_owned()],
                    group: Some(group.to_owned()),
                    concurrency: concurrency.map(limit),
                    ..ConsumerRegistration::default()
                },
            );
            registration
                .routing
                .insert(group.to_owned(), "all".to_owned());
        }
        registration
            .consumer
            .insert("evaluator".to_owned(), ConsumerRegistration::default());
        let router = |targets: &[&str], policy, concurrency| RouterDeclaration {
            targets: targets.iter().map(|&target| target.to_owned()).collect(),
            policy,
            concurrency: limit(concurrency),
            weights: BTreeMap::new(),
        };
        registration.router.insert(
            "all".to_owned(),
            router(
                &["explorers", "planner", "verifier"],
                RoutingPolicy::First,
                2,
            ),
        );
        registration.router.insert(
            "explorers".to_owned(),
            router(&["explorer", "fringe"], RoutingPolicy::Weighted, 1),
        );
        registration.validate().expect("valid");
        let capacity = |name: &str| {
            Capacity::new(
                state.path(),
                name,
                registration.consumer[name].concurrency,
                registration.route_for(name).as_ref(),
            )
            .expect("capacity")
        };
        let explorer = capacity("explorer");
        let fringe = capacity("fringe");
        let planner = capacity("planner");
        let verifier = capacity("verifier");

        let exploring = explorer.try_acquire().expect("files").expect("available");
        assert!(fringe.try_acquire().expect("files").is_none());
        let verifying = verifier.try_acquire().expect("files").expect("available");
        assert!(planner.try_acquire().expect("files").is_none());
        drop(exploring);
        let planning = planner.try_acquire().expect("files").expect("available");
        assert!(fringe.try_acquire().expect("files").is_none());
        drop(verifying);
        assert!(planner.try_acquire().expect("files").is_none());
        assert!(fringe.try_acquire().expect("files").is_some());
        drop(planning);
        assert!(planner.try_acquire().expect("files").is_some());

        // No limit at all: always room.
        assert!(registration.route_for("evaluator").is_none());
        let evaluator = capacity("evaluator");
        let _first = evaluator.try_acquire().expect("files").expect("available");
        assert!(evaluator.try_acquire().expect("files").is_some());
    }
}
