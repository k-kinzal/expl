//! Project-local reaction slots shared by every process using a limit.

use std::fmt::Write;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::declaration::Route;

pub(super) struct Capacity(Vec<(PathBuf, NonZeroUsize)>);

pub(super) struct Slot(File);

impl Drop for Slot {
    fn drop(&mut self) {
        // An unrelated fork may temporarily have a copy of the file
        // descriptor until it execs. Release the lock here, before close.
        drop(self.0.unlock());
    }
}

impl Capacity {
    pub(super) fn new(state: &Path, agent: &Agent, route: Option<&Route<'_>>) -> io::Result<Self> {
        let root = state.join("slots");
        let mut limits = Vec::new();
        if let Some(limit) = agent.declaration().concurrency {
            limits.push((directory(&root, "agent", agent.name()), limit));
        }
        if let Some(route) = route {
            limits.extend(
                route
                    .groups(agent.name())
                    .map(|(name, limit)| (directory(&root, "router", name), limit)),
            );
        }
        for (directory, _) in &limits {
            std::fs::create_dir_all(directory)?;
        }
        Ok(Self(limits))
    }

    /// Take one slot per limit without waiting. Dropping the slots frees all
    /// slots, including on failure or process exit. Event leases can expire
    /// independently without permitting two live reactions past a limit.
    pub(super) fn try_acquire(&self) -> io::Result<Option<Vec<Slot>>> {
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
    use super::*;
    use crate::{Declaration, Role};

    #[test]
    fn workers_need_room_at_each_level_and_release_slots_on_drop() {
        let project = tempfile::tempdir().expect("project");
        let declaration = Declaration::parse(&format!(
            "{}{}",
            Declaration::TEMPLATE,
            r#"
[routing]
exploration = "all"
planning = "all"
verification = "all"

[router.all]
targets = ["explorers", "planner", "verifier"]
policy = "first"
concurrency = 2

[router.explorers]
targets = ["explorer", "fringe"]
policy = "weighted"
concurrency = 1

[agent.fringe]
role = "exploration"
driver = "generic"
command = ["fringe"]
replicas = 1
breadth = 1
"#
        ))
        .expect("declaration");
        let capacity = |name| {
            let agent = Agent::from_declaration(name, declaration.agent(name).expect("agent"))
                .expect("driver");
            Capacity::new(
                project.path(),
                &agent,
                declaration.explicit_route(agent.role()).as_ref(),
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
        assert!(declaration.explicit_route(Role::Evaluation).is_none());
    }
}
