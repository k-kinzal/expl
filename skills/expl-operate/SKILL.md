---
name: expl-operate
description: Operate expl on this repository (expl developing itself) — start, watch, and steer the exploration/planning/execution/evaluation processes, manage goals, and keep the UI open. Use whenever you are the controller of a running expl, before adopting candidates (expl-adopt) or fixing expl itself (expl-troubleshoot).
---

# Operating expl (controller notes)

You are the **controller**. expl's agents explore, plan, execute and evaluate on
their own; you keep them running, read what they produce, adopt good candidates
into `main`, and fix expl when it stops behaving. You do not implement features
yourself — expl does. You only fix what blocks expl from continuing to explore.

## Standing rules (from the owner)

- Exploration, planning and evaluation run on **Claude Code and Codex, routed
  automatically**; execution runs on **cursor-agent with Claude Opus 5.5**
  (`claude-opus-5-5-high`). This is configured in `expl.toml`; keep it so.
- **Markdown files are frozen for expl's agents.** No candidate that adds, modifies
  or deletes a `*.md` file is ever merged. The controller edits Markdown only when
  the owner explicitly asks (they did for these skills, and for rewriting
  `AGENTS.md` when the vision changed) and commits it to `main` directly.
- Goals: **#2 "Achieve the vision in AGENTS.md"** (counter-exploration) and **#82
  "Explore questions with investigation steps, not only changes"** (counter-
  exploration) — the owner's direction that steps need not be commits: an
  investigation records a finding (yes / no / unknown + evidence) that converges on
  its goal, not the trunk. Do not retire either without being asked.
- `AGENTS.md` is the vision every agent reads. When the owner changes direction,
  update it (on their request) before adding goals, so evaluation judges against
  the new vision.
- Your job: control expl and merge into `main`. If expl misbehaves because of a
  bug, fix it (see `expl-troubleshoot`) so exploration keeps going.
- Keep `expl-ui` running so the owner can watch what is explored and adopted.

## How this repository is wired

| Agent | Role | CLI(s) | Replicas |
| --- | --- | --- | --- |
| explorer | exploration (mainstream, `weight = 4`) | claude_code ⇄ codex | 1 |
| fringe | exploration (niche + counterexamples, `weight = 1`) | claude_code ⇄ codex | 1 |
| planner | planning | claude_code ⇄ codex | 1 |
| executor | execution | cursor_agent (`--model claude-opus-5-5-high --force --trust --sandbox disabled`) | 1 |
| evaluator | evaluation | claude_code ⇄ codex | 1 |

- **Two kinds of exploration, routed by `weight`.** `explorer` enumerates the most
  promising directions; `fringe` deliberately widens the frame — niche directions
  inside the goal nobody asked for, inversions, and counterexamples (titles start
  with `Fringe:` / `Counterexample:`). Agents of one role that declare `weight`
  split that role's events: each event goes to exactly one of them (4:1 here), the
  other acknowledges it as `routed to …`. Agents without `weight` get every event.
  To add another exploration perspective, declare another exploration agent with a
  `weight` and its own `instructions`.
- **Explore wide, plan narrow.** Explorers list up to `breadth = 8` possibilities
  without filtering by likelihood; the planner ranks weak or counterproductive steps
  low and prunes only duplicates. `max_pending = 20` bounds the backlog.
- **One replica per agent, on purpose.** The owner prefers slow exploration over
  bursts of token use. Do not raise `replicas` without being asked.
- **Random revisits** (`revisit_cooldown = 600` on the explorer): when the explorer
  has no event to react to, it waits until 10 minutes have passed since its last
  reaction, then explores again from a random starting point — the active origin,
  a completed step not yet in the trunk, or a counter-exploration goal
  (`node.revisited` in `expl event ls`). Starting points are revisited again and
  again (routed by weight like any exploration event); each prompt lists the steps that already exist so new ones are different.
  `max_pending` still pauses it. Raise the cooldown if token use is too high.
- **Routing** (`[[agent.<name>.alternative]]`): each invocation starts at the next
  CLI in turn (per process, seeded by pid) and falls over to the other CLI when
  one fails (usage limit, outage, unusable reply). A run log shows which CLI ran
  in its first line: `=== agent `explorer` (codex) ===`, and
  `=== falling over to the next CLI ===` when it switched.
- All agents share `CARGO_TARGET_DIR=/Users/ab/Projects/expl/.expl/cargo-target`
  so worktrees do not rebuild gpui from scratch. Builds serialise on cargo's lock;
  that is expected, not a hang.
- The `expl` binary on PATH is `~/.cargo/bin/expl`, installed from `main` with
  `skills/expl-operate/scripts/install.sh [--ui]` (builds in `target/`, then
  copy-and-rename: plain `cp` over a binary makes macOS kill every later launch).
  Running processes keep the binary they started with until restarted.

## Where things are

| What | Where |
| --- | --- |
| graph, events, inboxes, processes | `.expl/expl.db` (SQLite) |
| process logs (one per process) | `.expl/proc/<id>.log` — `expl logs <name> [-f]` |
| agent run logs (prompt + raw output) | `.expl/logs/<run>-<agent>.log` — `expl node logs <id>` |
| step worktrees / scratch worktrees | `.expl/worktrees/node-<id>`, `scratch-*` |
| step branches | `expl/node-<id>` |
| UI log | `.expl/operator/expl-ui.log` |

## Start / stop

```sh
expl up                                   # idempotent: tops every agent up to `replicas`
nohup expl-ui -C "$PWD" > .expl/operator/expl-ui.log 2>&1 &   # the live graph window
# UI controls: scroll zooms around the pointer, drag pans, +/- zoom, f fits, 0 resets.
# Adopted work is drawn as a branch merging into the trunk spine; merge origins have
# a halo and a "+N" in their label.
expl down -t 30                           # stop all (in-flight runs are interrupted and redelivered)
expl run executor -d                      # one extra process of an agent
```

`skills/expl-operate/scripts/rolling-restart.sh` restarts every process that runs
an older binary than the installed `expl`, each when it is idle (or has just moved
on to a new event), so in-flight agent runs are not thrown away. Use it after every
install of a new binary. `skills/expl-operate/scripts/watch.sh` streams the log lines worth
acting on (use it as a Monitor; `tail -F` misses processes started later on macOS).

Check the UI is alive with `pgrep -fl expl-ui`; restart it with the line above if not.

## Watching

`skills/expl-operate/scripts/status.sh` prints everything at once. Individually:

```sh
expl ps                       # what each process is doing right now
expl node ls                  # proposed → planned → ready → running → completed/failed
expl path ls                  # the latest plan
expl candidate ls             # steps evaluated as satisfying a goal (adoption list)
expl origin ls                # lineage: each trunk position, what it adopted, trunk scores
expl event ls --limit 30      # event log
expl event ls --agent planner # how one agent's deliveries stand (failed / leased / acked)
expl node inspect <id>        # edges, evaluations, agent runs of one node
expl node logs <id>           # newest raw agent log about a node
```

Typical rhythm: look every few minutes; act when (a) a candidate appears or a step
scores high → `expl-adopt`; (b) a delivery failed, a process exited, or nothing has
moved for long → `expl-troubleshoot`.

Note: exploration pauses itself while `max_pending` (20) steps wait for execution
(`paused: N steps pending` in `expl ps`). That is back-pressure, not a bug. A newly
added goal is still explored at once while paused (`goal.added` is unpausable).

## Goals

```sh
expl goal ls -a
expl goal add "TITLE" --objective "measurable condition" [--counter]
expl goal retire <id>
```

Write objectives as checkable conditions (tests pass, behaviour X observable).
Add new goals only when the owner asks or when the vision goal clearly needs a
sharper sub-perspective; say so to the owner when you do.

## Steering without editing code

- `expl node prune <id>` — discard a step that is off-track (e.g. it is about docs).
- `expl node retry <id>` — re-plan a failed step after you fixed its cause.
- `expl node add "TITLE" --after <id> --priority N` — hand-propose a step (rarely).
- Edit agent `instructions` in `expl.toml`, commit it to `main`, then restart the
  affected agent's processes (`expl stop <name>` then `expl up`) — declarations are
  read when a process starts.
