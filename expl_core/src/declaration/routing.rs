//! Explicit role assignments and nested agent routers.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use super::{Declaration, DeclarationError, spread};
use crate::event::EventId;
use crate::role::Role;

/// One `[router.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDeclaration {
    /// Agents or routers, in policy order.
    pub targets: Vec<String>,
    /// How to choose among targets that serve the role being routed.
    pub policy: RoutingPolicy,
    /// Maximum concurrent reactions in this router's assigned group.
    pub concurrency: NonZeroUsize,
    /// Relative shares under the weighted policy. Omitted targets have weight one.
    #[serde(default)]
    pub weights: BTreeMap<String, u32>,
}

/// How a router chooses an eligible target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    /// Always choose the first eligible target in declaration order.
    First,
    /// Split events in proportion to the eligible targets' weights.
    Weighted,
}

type Members<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

/// The reachable targets for a role's explicit assignment.
pub(crate) struct Route<'a> {
    pub(crate) role: Role,
    root: &'a str,
    declaration: &'a Declaration,
    members: Members<'a>,
}

impl Route<'_> {
    pub(crate) fn members(&self) -> &BTreeSet<&str> {
        &self.members[self.root]
    }

    pub(crate) fn groups(&self, agent: &str) -> impl Iterator<Item = (&str, NonZeroUsize)> {
        self.members.iter().filter_map(move |(name, members)| {
            let router = self.declaration.router.get(*name)?;
            members
                .contains(agent)
                .then_some((*name, router.concurrency))
        })
    }

    pub(crate) fn select(&self, event: EventId) -> &str {
        let mut target = self.root;
        while let Some(router) = self.declaration.router.get(target) {
            let eligible = || {
                router
                    .targets
                    .iter()
                    .filter(|name| !self.members[name.as_str()].is_empty())
            };
            let weight = |name: &String| u64::from(*router.weights.get(name).unwrap_or(&1));
            target = match router.policy {
                RoutingPolicy::First => eligible().next(),
                RoutingPolicy::Weighted => {
                    // Mix the router name into the event id so nested policies
                    // do not pick correlated subsets of the same events.
                    let seed = target
                        .bytes()
                        .fold(event.0 as u64, |key, byte| spread(key ^ u64::from(byte)));
                    let mut point = spread(seed) % eligible().map(weight).sum::<u64>();
                    eligible().find(|name| {
                        let share = weight(name);
                        if point < share {
                            true
                        } else {
                            point -= share;
                            false
                        }
                    })
                }
            }
            .expect("the route has an eligible target");
        }
        target
    }
}

impl Declaration {
    pub(super) fn validate_routing(&self) -> Result<(), DeclarationError> {
        for (name, router) in &self.router {
            let invalid = |why| DeclarationError::Invalid(format!("router `{name}`: {why}"));
            if self.agent.contains_key(name) {
                return Err(invalid("name is also an agent"));
            }
            let targets: BTreeSet<_> = router.targets.iter().collect();
            if targets.is_empty() || targets.len() != router.targets.len() {
                return Err(invalid("`targets` must be nonempty and unique"));
            }
            if !router.weights.is_empty() && router.policy != RoutingPolicy::Weighted {
                return Err(invalid("`weights` require the weighted policy"));
            }
            if router
                .weights
                .iter()
                .any(|(target, weight)| *weight == 0 || !targets.contains(target))
            {
                return Err(invalid(
                    "`weights` must be positive and name targets of this router",
                ));
            }
        }
        let mut members = BTreeMap::new();
        for name in self.router.keys() {
            self.collect_members(name, &mut members, &mut Vec::new())?;
        }
        for (role, root) in &self.routing {
            let agents = self.collect_members(root, &mut members, &mut Vec::new())?;
            if !agents.iter().any(|agent| self.agent[*agent].role == *role) {
                return Err(DeclarationError::Invalid(format!(
                    "routing for `{role}`: `{root}` has no agent for this role"
                )));
            }
        }
        Ok(())
    }

    fn collect_members<'a>(
        &'a self,
        target: &'a str,
        members: &mut Members<'a>,
        path: &mut Vec<&'a str>,
    ) -> Result<BTreeSet<&'a str>, DeclarationError> {
        if let Some(known) = members.get(target) {
            return Ok(known.clone());
        }
        if path.contains(&target) {
            path.push(target);
            return Err(DeclarationError::Invalid(format!(
                "router cycle: {}",
                path.join(" -> ")
            )));
        }
        let agents = if self.agent.contains_key(target) {
            BTreeSet::from([target])
        } else if let Some(router) = self.router.get(target) {
            path.push(target);
            let mut agents = BTreeSet::new();
            for child in &router.targets {
                agents.extend(self.collect_members(child, members, path)?);
            }
            path.pop();
            agents
        } else {
            return Err(DeclarationError::Invalid(format!(
                "unknown routing target `{target}`"
            )));
        };
        members.insert(target, agents.clone());
        Ok(agents)
    }

    /// Whether a declared agent can receive events under this role assignment.
    #[must_use]
    pub fn is_assigned(&self, agent: &str) -> bool {
        self.agent.get(agent).is_some_and(|declaration| {
            self.explicit_route(declaration.role)
                .is_none_or(|route| route.members().contains(agent))
        })
    }

    pub(crate) fn explicit_route(&self, role: Role) -> Option<Route<'_>> {
        let root = self.routing.get(&role)?;
        let mut members = BTreeMap::new();
        self.collect_members(root, &mut members, &mut Vec::new())
            .expect("validated routing targets");
        for agents in members.values_mut() {
            agents.retain(|agent| self.agent[*agent].role == role);
        }
        Some(Route {
            role,
            root,
            declaration: self,
            members,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTERS: &str = r#"
[routing]
exploration = "all"
verification = "all"

[router.all]
targets = ["verifier", "explorers"]
policy = "first"
concurrency = 2

[router.explorers]
targets = ["explorer", "fringe"]
policy = "weighted"
weights = { explorer = 4 }
concurrency = 1

[agent.fringe]
role = "exploration"
driver = "generic"
command = ["fringe"]
replicas = 1
breadth = 2
"#;

    #[test]
    fn nested_routing_applies_policies_after_filtering_for_the_role() {
        let declaration = Declaration::parse(&format!("{}{ROUTERS}", Declaration::TEMPLATE))
            .expect("declaration");
        let exploration = declaration
            .explicit_route(Role::Exploration)
            .expect("route");
        let verification = declaration
            .explicit_route(Role::Verification)
            .expect("route");
        let fringes = (1..=4000)
            .filter(|id| exploration.select(EventId(*id)) == "fringe")
            .count();
        assert!((700..900).contains(&fringes), "{fringes}");
        assert_eq!(verification.select(EventId(1)), "verifier");
        assert_eq!(
            exploration
                .groups("explorer")
                .map(|(name, n)| (name, n.get()))
                .collect::<Vec<_>>(),
            [("all", 2), ("explorers", 1)]
        );
        assert_eq!(verification.groups("verifier").count(), 1);
    }

    #[test]
    fn routing_rejects_invalid_graphs_and_limits() {
        let base = format!("{}{ROUTERS}", Declaration::TEMPLATE);
        for (from, to, problem) in [
            (
                "\"verifier\", \"explorers\"",
                "\"missing\"",
                "unknown routing target",
            ),
            (
                "\"explorer\", \"fringe\"",
                "\"explorer\", \"fringe\", \"all\"",
                "router cycle",
            ),
            (
                "exploration = \"all\"",
                "exploration = \"verifier\"",
                "no agent for this role",
            ),
            ("explorer = 4", "explorer = 0", "weights"),
            ("concurrency = 2", "concurrency = 0", "concurrency"),
        ] {
            let error = Declaration::parse(&base.replacen(from, to, 1)).expect_err(problem);
            assert!(error.to_string().contains(problem), "{error}");
        }
    }
}
