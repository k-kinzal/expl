//! What a worker executes: `expl react`, `expl follow`, `expl revisit`.
//!
//! The delivery loop is the runtime's and holds no code of this explorer.
//! For each event it leases, a worker runs `expl react <CONSUMER>`, which
//! makes one reaction of the consumer's handler and reports what became of
//! it. Between events it runs `expl follow`, so the origin follows the
//! trunk, and an idle exploration runs `expl revisit <CONSUMER>`.

use std::path::Path;

use expl_core::{Advance, Graph, Interrupt, Trunk};
use expl_runtime::reaction::{self, Outcome, Reaction};
use expl_runtime::telemetry;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, KeyValue};

use crate::dispatch::load;
use crate::error::CliError;
use crate::output::{json_pretty, println};
use crate::telemetry::Telemetry;

/// React, as `consumer`, to the delivery described in the environment.
pub(crate) fn react(start: &Path, consumer: &str) -> Result<(), CliError> {
    let leased = Reaction::from_env().map_err(|error| CliError::Usage(error.to_string()))?;
    // A declaration or agent control that cannot be read stands in the way
    // of every reaction alike: the event is given back, not charged with a
    // failed attempt, and is delivered once the file is put right.
    let give_back = |error: &dyn std::fmt::Display| {
        leased
            .report(&Outcome::Release {
                note: Some(error.to_string()),
            })
            .ok();
    };
    let context = load(start).inspect_err(|error| give_back(error))?;
    // The consumer's CLIs take turns across its worker's reactions.
    let consumer = context
        .consumer(consumer)
        .inspect_err(|error| give_back(error))?
        .starting_at(leased.turn);
    let project = context.project();
    let _telemetry = Telemetry::start(project, context.declaration().telemetry.as_ref(), false);
    let interrupt = leased
        .interrupt(&project.database_path())
        .map_err(|e| CliError::io("install signal handlers", e))?;

    // The reaction's spans continue the trace of the worker's delivery.
    let delivery = std::env::var(reaction::TRACEPARENT)
        .ok()
        .and_then(|parent| {
            telemetry::remote_span(&parent, std::env::var(reaction::TRACESTATE).ok().as_deref())
        });
    let _delivery = delivery.map(|span| Context::new().with_remote_span_context(span).attach());
    let event = &leased.delivery.event;
    let mut attributes = vec![
        KeyValue::new("expl.consumer", consumer.name().to_owned()),
        KeyValue::new("expl.handler", consumer.declaration().handler.as_str()),
        KeyValue::new("expl.event.kind", event.kind.clone()),
        KeyValue::new("expl.event.id", event.id.0),
        KeyValue::new("expl.delivery.attempt", leased.delivery.attempts),
        KeyValue::new("expl.reaction.mode", leased.mode.as_str()),
    ];
    if let Some(node) = event.subject {
        attributes.push(KeyValue::new("expl.node.id", node));
    }
    let span = telemetry::span("expl.reaction", attributes);
    let scope = span.clone().attach();

    let outcome = expl_core::react::react(&context, &consumer, &leased, &interrupt);
    if outcome.is_err() {
        telemetry::error(&span, "reaction");
    }
    span.span().end();
    drop(scope);
    let outcome = outcome?;
    leased
        .report(&outcome)
        .map_err(|e| CliError::io("report the reaction's outcome", e))?;
    if leased.outcome.is_none() {
        // Run by hand: nobody reads an outcome file.
        json_pretty(&outcome)?;
    }
    Ok(())
}

/// Look once whether the trunk moved, and say so on stdout: a worker logs
/// what this prints.
pub(crate) fn follow(start: &Path) -> Result<(), CliError> {
    let context = load(start)?;
    let project = context.project();
    let declaration = context.declaration();
    let interrupt =
        Interrupt::on_signals().map_err(|e| CliError::io("install signal handlers", e))?;
    let mut graph = Graph::open(&project.database_path())?;
    let trunk = Trunk::new(project, declaration.trunk.as_deref()).with_remote(
        project,
        declaration.workspace.remote.as_deref(),
        &interrupt,
    );
    match trunk.follow(&mut graph) {
        Ok(Some(advance)) => println(&moved(&advance)),
        Ok(None) => Ok(()),
        Err(error) => {
            // This runs every few seconds: telemetry is started only when
            // there is something to report.
            let _telemetry = Telemetry::start(project, declaration.telemetry.as_ref(), false);
            let span = telemetry::span("expl.trunk.follow", Vec::new());
            telemetry::error(&span, "trunk");
            println(&format!("cannot follow the trunk: {error}"))
        }
    }
}

/// One line about a trunk that moved.
fn moved(advance: &Advance) -> String {
    let adopted = if advance.adopted.is_empty() {
        "nothing adopted".to_owned()
    } else {
        format!(
            "adopted {}",
            advance
                .adopted
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    format!(
        "trunk {} moved to {}: origin {} supersedes {}, {adopted}{}",
        advance.branch,
        advance.commit.chars().take(7).collect::<String>(),
        advance.origin,
        advance.previous,
        if advance.descends {
            ""
        } else {
            " (the trunk was rewritten)"
        }
    )
}

/// Record `node.revisited` for a random starting point of `consumer`'s
/// exploration.
pub(crate) fn revisit(start: &Path, consumer: &str) -> Result<(), CliError> {
    let context = load(start)?;
    let declared = context.declaration().consumer(consumer)?;
    let project = context.project();
    let _telemetry = Telemetry::start(project, context.declaration().telemetry.as_ref(), false);
    let mut graph = Graph::open(&project.database_path())?;
    expl_core::react::revisit(&mut graph, declared.speculate == Some(true))?;
    Ok(())
}
