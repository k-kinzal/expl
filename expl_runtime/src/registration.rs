//! What an explorer registers with the framework: its consumers (which
//! events, what to execute for one of them, and where), the periodic tasks
//! beside them, and how events are shared between consumers.
//!
//! Each consumer receives every event it subscribes to by default.
//! Consumers with the same `group` share its events: the group is routed
//! to one consumer, or to a [router](RouterDeclaration) that chooses one
//! recipient per event among the consumers that subscribe to it.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::EventId;

/// A registration is well-formed but inconsistent.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistrationError {
    /// A router is declared in a way that cannot route.
    #[error("router `{router}`: {reason}")]
    Router {
        /// The router.
        router: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Routers name each other in a circle.
    #[error("router cycle: {0}")]
    Cycle(String),
    /// A routing target is neither a consumer nor a router.
    #[error("unknown routing target `{0}`")]
    UnknownTarget(String),
    /// A group is routed to a target none of its consumers is under.
    #[error("routing for `{group}`: `{root}` has no consumer in this group")]
    EmptyGroup {
        /// The delivery group.
        group: String,
        /// Where it is routed to.
        root: String,
    },
    /// A consumer names a group that is not routed anywhere.
    #[error("consumer `{consumer}`: unknown delivery group `{group}`")]
    UnknownGroup {
        /// The consumer.
        consumer: String,
        /// The group it names.
        group: String,
    },
}

/// Everything one explorer registers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    /// Named event consumers.
    #[serde(default)]
    pub consumer: BTreeMap<String, ConsumerRegistration>,
    /// Named routers, whose targets are consumers or other routers.
    #[serde(default)]
    pub router: BTreeMap<String, RouterDeclaration>,
    /// Name of each delivery group and its routing target.
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
    /// Commands run periodically beside the consumers.
    #[serde(default)]
    pub task: BTreeMap<String, TaskRegistration>,
    /// The pool whose shared limits this explorer's work counts against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

/// One consumer: when to react, what to execute, and where.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerRegistration {
    /// The event kinds that trigger this consumer.
    #[serde(default)]
    pub on: Vec<String>,
    /// Program and arguments executed for one delivered event.
    #[serde(default)]
    pub run: Vec<String>,
    /// Where the command runs.
    #[serde(default)]
    pub placement: Placement,
    /// Optional shared delivery group, routed under [`Registration::routing`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// How many workers serve this consumer.
    #[serde(default)]
    pub replicas: usize,
    /// Maximum concurrent reactions across this consumer's workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<NonZeroUsize>,
    /// At most one reaction at a time, whatever the number of workers.
    #[serde(default)]
    pub exclusive: bool,
    /// One reaction covers every subscribed event recorded before it.
    #[serde(default)]
    pub coalesce: bool,
    /// A command to run once the consumer has had nothing to react to for
    /// a while.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleCommand>,
    /// Extra environment variables for the command.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// What the explorer wants said about this consumer on its workers'
    /// spans and metrics, beside its name.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Where a consumer's command runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// On the elected leader, beside the explorer's database.
    #[default]
    Leader,
    /// On any live member.
    Any,
}

/// What an idle consumer runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleCommand {
    /// Seconds without a reaction before the command runs.
    pub after: u64,
    /// Program and arguments.
    pub run: Vec<String>,
}

/// A command run periodically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRegistration {
    /// Seconds between runs.
    pub every: u64,
    /// Program and arguments.
    pub run: Vec<String>,
}

/// One router: how a group's events are shared among its targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDeclaration {
    /// Consumers or routers, in policy order.
    pub targets: Vec<String>,
    /// How to choose among targets that subscribe to the event being routed.
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

/// The reachable targets for one delivery group.
#[derive(Debug)]
pub struct Route<'a> {
    group: &'a str,
    root: &'a str,
    registration: &'a Registration,
    members: Members<'a>,
}

impl Route<'_> {
    /// The delivery group this route serves.
    #[must_use]
    pub fn group(&self) -> &str {
        self.group
    }

    /// The consumers of the group that the route can reach.
    #[must_use]
    pub fn members(&self) -> &BTreeSet<&str> {
        &self.members[self.root]
    }

    /// The routers `consumer` is under, each with its concurrency limit.
    pub fn groups(&self, consumer: &str) -> impl Iterator<Item = (&str, NonZeroUsize)> {
        self.members.iter().filter_map(move |(name, members)| {
            let router = self.registration.router.get(*name)?;
            members
                .contains(consumer)
                .then_some((*name, router.concurrency))
        })
    }

    fn subscribes(&self, consumer: &str, kind: &str) -> bool {
        self.registration
            .consumer
            .get(consumer)
            .is_some_and(|consumer| consumer.on.iter().any(|on| on == kind))
    }

    /// Eligible owners per event kind as a JSON object, so SQLite can
    /// detect stranded assignments even when a former owner remains
    /// subscribed to other kinds. A kind no member subscribes to is absent.
    #[must_use]
    pub fn eligible_owners(&self) -> String {
        let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for consumer in self.members() {
            let Some(registered) = self.registration.consumer.get(*consumer) else {
                continue;
            };
            for kind in &registered.on {
                owners.entry(kind.as_str()).or_default().push(consumer);
            }
        }
        serde_json::json!(owners).to_string()
    }

    /// The consumer an event of `kind` goes to. Depends only on the event
    /// and the registration.
    ///
    /// # Panics
    ///
    /// When no member of the route subscribes to `kind`: only events a
    /// member subscribes to are routed.
    #[must_use]
    pub fn select(&self, kind: &str, event: EventId) -> &str {
        let mut target = self.root;
        while let Some(router) = self.registration.router.get(target) {
            let eligible = || {
                router.targets.iter().filter(|name| {
                    self.members[name.as_str()]
                        .iter()
                        .any(|consumer| self.subscribes(consumer, kind))
                })
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

impl Registration {
    /// Check that groups and routers are consistent.
    ///
    /// # Errors
    ///
    /// [`RegistrationError`] naming the first inconsistency.
    pub fn validate(&self) -> Result<(), RegistrationError> {
        for (name, consumer) in &self.consumer {
            if let Some(group) = &consumer.group
                && !self.routing.contains_key(group)
            {
                return Err(RegistrationError::UnknownGroup {
                    consumer: name.clone(),
                    group: group.clone(),
                });
            }
        }
        for (name, router) in &self.router {
            let invalid = |reason| RegistrationError::Router {
                router: name.clone(),
                reason,
            };
            if self.consumer.contains_key(name) {
                return Err(invalid("name is also a consumer"));
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
        for (group, root) in &self.routing {
            let consumers = self.collect_members(root, &mut members, &mut Vec::new())?;
            if !consumers.iter().any(|name| {
                self.consumer
                    .get(*name)
                    .is_some_and(|consumer| consumer.group.as_ref() == Some(group))
            }) {
                return Err(RegistrationError::EmptyGroup {
                    group: group.clone(),
                    root: root.clone(),
                });
            }
        }
        Ok(())
    }

    fn collect_members<'a>(
        &'a self,
        target: &'a str,
        members: &mut Members<'a>,
        path: &mut Vec<&'a str>,
    ) -> Result<BTreeSet<&'a str>, RegistrationError> {
        if let Some(known) = members.get(target) {
            return Ok(known.clone());
        }
        if path.contains(&target) {
            path.push(target);
            return Err(RegistrationError::Cycle(path.join(" -> ")));
        }
        let consumers = if self.consumer.contains_key(target) {
            BTreeSet::from([target])
        } else if let Some(router) = self.router.get(target) {
            path.push(target);
            let mut consumers = BTreeSet::new();
            for child in &router.targets {
                consumers.extend(self.collect_members(child, members, path)?);
            }
            path.pop();
            consumers
        } else {
            return Err(RegistrationError::UnknownTarget(target.to_owned()));
        };
        members.insert(target, consumers.clone());
        Ok(consumers)
    }

    /// Whether this consumer is reachable through its delivery group's
    /// router (or shares no group, and so receives everything itself).
    #[must_use]
    pub fn is_assigned(&self, name: &str) -> bool {
        self.consumer.contains_key(name)
            && self
                .route_for(name)
                .is_none_or(|route| route.members().contains(name))
    }

    /// The route of the delivery group `name` shares, if it shares one.
    /// `None` too for a registration whose routing is not
    /// [valid](Self::validate).
    #[must_use]
    pub fn route_for(&self, name: &str) -> Option<Route<'_>> {
        let group = self.consumer.get(name)?.group.as_deref()?;
        let root = self.routing.get(group)?;
        let mut members = BTreeMap::new();
        self.collect_members(root, &mut members, &mut Vec::new())
            .ok()?;
        for consumers in members.values_mut() {
            consumers.retain(|name| {
                self.consumer
                    .get(*name)
                    .is_some_and(|consumer| consumer.group.as_deref() == Some(group))
            });
        }
        Some(Route {
            group,
            root,
            registration: self,
            members,
        })
    }
}

/// `SplitMix64`: consecutive event ids land far apart, so routing by weight
/// does not alternate in lockstep with the event log.
fn spread(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consumer(on: &[&str], group: &str) -> ConsumerRegistration {
        ConsumerRegistration {
            on: on.iter().map(|&kind| kind.to_owned()).collect(),
            group: Some(group.to_owned()),
            replicas: 1,
            ..ConsumerRegistration::default()
        }
    }

    fn router(targets: &[&str], policy: RoutingPolicy, concurrency: usize) -> RouterDeclaration {
        RouterDeclaration {
            targets: targets.iter().map(|&target| target.to_owned()).collect(),
            policy,
            concurrency: NonZeroUsize::new(concurrency).expect("limit"),
            weights: BTreeMap::new(),
        }
    }

    /// Two explorers and a verifier under one router, the explorers split
    /// four to one under a nested one; a planner outside every group.
    fn nested() -> Registration {
        let mut registration = Registration::default();
        registration.consumer.insert(
            "explorer".to_owned(),
            consumer(&["goal.added", "node.evaluated"], "exploration"),
        );
        registration.consumer.insert(
            "fringe".to_owned(),
            consumer(&["goal.added"], "exploration"),
        );
        registration.consumer.insert(
            "verifier".to_owned(),
            consumer(&["node.ready"], "verification"),
        );
        registration.consumer.insert(
            "planner".to_owned(),
            ConsumerRegistration {
                on: vec!["node.proposed".to_owned()],
                replicas: 1,
                ..ConsumerRegistration::default()
            },
        );
        registration.router.insert(
            "all".to_owned(),
            router(&["verifier", "explorers"], RoutingPolicy::First, 2),
        );
        let mut explorers = router(&["explorer", "fringe"], RoutingPolicy::Weighted, 1);
        explorers.weights.insert("explorer".to_owned(), 4);
        registration
            .router
            .insert("explorers".to_owned(), explorers);
        for group in ["exploration", "verification"] {
            registration
                .routing
                .insert(group.to_owned(), "all".to_owned());
        }
        registration
    }

    #[test]
    fn nested_routing_applies_policies_after_filtering_for_the_event_and_group() {
        let registration = nested();
        registration.validate().expect("valid");
        let exploration = registration.route_for("explorer").expect("route");
        let verification = registration.route_for("verifier").expect("route");
        assert_eq!(exploration.group(), "exploration");
        let fringes = (1..=4000)
            .filter(|id| exploration.select("goal.added", EventId(*id)) == "fringe")
            .count();
        assert!((700..900).contains(&fringes), "{fringes}");
        assert_eq!(verification.select("node.ready", EventId(1)), "verifier");
        for id in 1..=100 {
            assert_eq!(
                exploration.select("node.evaluated", EventId(id)),
                "explorer"
            );
        }
        assert_eq!(
            exploration
                .groups("explorer")
                .map(|(name, n)| (name, n.get()))
                .collect::<Vec<_>>(),
            [("all", 2), ("explorers", 1)]
        );
        assert_eq!(verification.groups("verifier").count(), 1);
        // Only the kinds a member subscribes to have owners.
        let owners: serde_json::Value =
            serde_json::from_str(&exploration.eligible_owners()).expect("json");
        assert_eq!(
            owners,
            serde_json::json!({
                "goal.added": ["explorer", "fringe"],
                "node.evaluated": ["explorer"],
            })
        );
        assert!(registration.is_assigned("fringe"));
        // Outside every group: nothing to be assigned by, so it serves itself.
        assert!(registration.route_for("planner").is_none());
        assert!(registration.is_assigned("planner"));
        assert!(!registration.is_assigned("missing"));
    }

    #[test]
    fn routing_rejects_invalid_graphs_and_limits() {
        let mut unknown = nested();
        unknown.router.get_mut("all").expect("router").targets = vec!["missing".to_owned()];
        assert_eq!(
            unknown.validate(),
            Err(RegistrationError::UnknownTarget("missing".to_owned()))
        );

        let mut cycle = nested();
        cycle
            .router
            .get_mut("explorers")
            .expect("router")
            .targets
            .push("all".to_owned());
        let error = cycle.validate().expect_err("cycle");
        assert!(error.to_string().contains("router cycle"), "{error}");

        let mut empty = nested();
        empty
            .routing
            .insert("exploration".to_owned(), "verifier".to_owned());
        let error = empty.validate().expect_err("no consumer of the group");
        assert!(
            error.to_string().contains("no consumer in this group"),
            "{error}"
        );
        // An unreachable consumer is declared, but assigned nothing.
        assert!(!empty.is_assigned("explorer"));

        let mut weights = nested();
        weights
            .router
            .get_mut("explorers")
            .expect("router")
            .weights
            .insert("explorer".to_owned(), 0);
        let error = weights.validate().expect_err("zero weight");
        assert!(error.to_string().contains("weights"), "{error}");

        let mut group = nested();
        group.consumer.get_mut("planner").expect("consumer").group = Some("planning".to_owned());
        assert_eq!(
            group.validate(),
            Err(RegistrationError::UnknownGroup {
                consumer: "planner".to_owned(),
                group: "planning".to_owned()
            })
        );

        let zero = serde_json::from_value::<RouterDeclaration>(serde_json::json!({
            "targets": ["explorer"], "policy": "first", "concurrency": 0
        }));
        assert!(zero.is_err(), "a router needs room for one reaction");
    }
}
