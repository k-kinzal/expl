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
- **Markdown files are frozen.** No candidate that adds, modifies or deletes a
  `*.md` file is ever merged. Do not edit `AGENTS.md`, `README.md`, `CLAUDE.md`
  yourself either. (These skills are the one exception the owner asked for.)
- The first goal is **"Achieve the vision in AGENTS.md"** (goal #2, with
  counter-exploration). Do not retire it.
- Your job: control expl and merge into `main`. If expl misbehaves because of a
  bug, fix it (see `expl-troubleshoot`) so exploration keeps going.
- Keep `expl-ui` running so the owner can watch what is explored and adopted.

## How this repository is wired

| Agent | Role | CLI(s) | Replicas |
| --- | --- | --- | --- |
| explorer | exploration | claude_code ⇄ codex | 1 |
| planner | planning | claude_code ⇄ codex | 1 |
| executor | execution | cursor_agent (`--model claude-opus-5-5-high --force --trust --sandbox disabled`) | 2 |
| evaluator | evaluation | claude_code ⇄ codex | 1 |

- **Routing** (`[[agent.<name>.alternative]]`): each invocation starts at the next
  CLI in turn (per process, seeded by pid) and falls over to the other CLI when
  one fails (usage limit, outage, unusable reply). A run log shows which CLI ran
  in its first line: `=== agent `explorer` (codex) ===`, and
  `=== falling over to the next CLI ===` when it switched.
- All agents share `CARGO_TARGET_DIR=/Users/ab/Projects/expl/.expl/cargo-target`
  so worktrees do not rebuild gpui from scratch. Builds serialise on cargo's lock;
  that is expected, not a hang.
- The `expl` binary on PATH is `~/.cargo/bin/expl` (installed from `main`).
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
expl down -t 30                           # stop all (in-flight runs are interrupted and redelivered)
expl run executor -d                      # one extra process of an agent
```

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

Note: exploration pauses itself while `max_pending` (6) steps wait for execution
(`paused: N steps pending` in `expl ps`). That is back-pressure, not a bug.

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
