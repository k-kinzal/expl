# AGENTS

## Vision

Expl is an agentic tool for exploring a problem space.
It is the successor tool to k-kinzal/iter, designed to reach broadly and in parallel across the problem space to explore every possibility and find the best goals.

The fundamental algorithm of Expl is based on a graph structure.
An exploration agent sequentially identifies nodes that could potentially lead from a specific point to a goal node. Depending on the goal setting, it can also act as a counter-exploration agent to search for possibilities in the direction opposite to the goal.
A planning agent defines paths of node combinations found through exploration and sets node priorities.
An execution agent executes the nodes according to their priorities.
An evaluation agent evaluates the completed nodes and determines whether the goal has been satisfied.
Nodes are linked with Git, representing specific branches and working areas that achieve the objectives set by the exploration agent.
There is not necessarily only one goal; depending on the level of abstraction of the goal, multiple problem resolutions may each be evaluated as satisfying the goal. Furthermore, nodes are not mutually exclusive, and paths may merge, such as A -> C and B -> C.

Due to this nature, Expl may find the achievement of a goal, but the exploration will never stop. Selecting a goal is the responsibility of a human, and Expl's responsibility is to generate those candidates.

## Relations

- [k-kinzal/iter](https://github.com/k-kinzal/iter)

## Core Modeling

### The nouns

expl's domain is a graph and the roles that grow it. Names below are the names used in code; keep them.

- **Graph** — the exploration graph, persisted in SQLite (`.expl/expl.db`). The single source of truth every role reads and writes (`expl_core::graph`).
- **Node** — a state in the problem space. Three kinds: the one **origin** (the commit exploration started from), **steps** (units of work, each realised as a commit on its own branch `expl/node-<id>`), and **goals** (what a human wants; evaluated against, never executed). Step lifecycle: `proposed → planned → running → completed | failed`, or `pruned`. Goals are `active` until retired.
- **Edge** — `source → target`: the target builds on the source. Edges form a DAG. Paths merge (`A → C`, `B → C`): the target's branch starts from a merge of its predecessors' branches. An edge into a goal marks which goal a step heads for.
- **Plan** / **PlannedPath** — the planner's latest ordered combinations of steps toward each goal. Priorities live on the nodes.
- **Evaluation** — one verdict (satisfied, score 0–100, reason) on one completed step against one goal. Satisfied evaluations are the **candidates** a human chooses from.
- **AgentRunRecord** — the journal of agent invocations (role, node, agent, status, raw log path).

### The roles

Four responsibilities, one agent invocation each, with a fixed contract (`expl_core::role`):

| Role | Runs in | Reads | Writes |
| --- | --- | --- | --- |
| **Explorer** | the starting node's worktree (read-only) | graph, goals | proposed steps + edges |
| **Planner** | the origin's worktree (read-only) | graph, evaluations | plan, priorities, prunes |
| **Executor** | the step's own worktree | the step and its ancestors | a commit on the step's branch |
| **Evaluator** | the step's worktree (read-only) | the step, its diff, goals | evaluations |

Exploration has a **Direction**: `forward` from a realised node toward the goals, or `backward` from a goal (**counter-exploration**, enabled per goal with `expl goal add --counter`) to find what must hold just before it. Every role asks its agent for a JSON response validated against a schema; roles never print and never decide when they run.

### Agents and workspaces

These mirror iter and keep its naming:

- **Agent** — a struct that realises the invocation cycle once (scratch dir, spawn in the workspace directory, prompt on stdin, output teed to the run log, interpretation). **AgentDriver** — the bidirectional translator for one CLI: `command` encodes the request, `interpret` decodes the CLI's verdict into an **AgentRun**. Drivers never touch a process. Drivers: `ClaudeCodeDriver`, `CodexDriver`, `GenericDriver`.
- **Workspaces** — the enduring collaborator owning `.expl/worktrees/`. **Workspace** — one node's worktree: `create` (from predecessors, merging joins), `commit`, `restore`.

### Runner

The **Runner** repeats **cycles**: explore (forward + counter) → plan → execute (up to `run.parallelism` ready steps in parallel) → evaluate. It stops only on a cycle budget or when a cycle finds nothing to do. Role failures are reported and the cycle continues; graph failures stop it. Commands that run agents hold the **project lock** (`.expl/lock`); a run re-plans steps left `running` by a crashed run.

### The concept map

- **The declaration** — `expl.toml` at the repository root: agents, role bindings, run settings. No implicit defaults; `expl init` writes a complete template (`expl_core::declaration`).
- **The project** — a git repository's top level plus its state directory `.expl/` (graph, worktrees, logs, lock), excluded via `.git/info/exclude`. Discovery is bounded by the git working tree, so an agent inside a node worktree cannot reach the enclosing project's graph (`expl_core::project`).
- **The run** — graph, roles, agents, workspaces, runner (`expl_core`).
- **Operating** — the human's surface (`expl_cli`): `<resource> <verb>` commands, stdout for data, stderr for status, iter's exit codes (`0` ok, `1` user input, `2` runtime, `64` configuration).

Crate dependencies run one way: `expl_cli` → `expl_core`. Nothing depends on `expl_cli`.

## Development

- `cargo fmt`, `cargo clippy --all-targets` (pedantic, denied), and `cargo test` must pass before finishing any change.
- Errors are typed with `thiserror`; `anyhow` is test-only. Library code never prints.
- `expl_cli/tests/cycle.rs` drives every role end to end with a scripted `generic` agent; extend it when behaviour changes.

### Developing expl with expl

```sh
cargo install --path expl_cli   # or use target/release/expl
expl init                       # once; origin = HEAD
expl goal add "<goal>" --objective "<verifiable statement>" [--counter]
expl run --cycles 3             # explore → plan → execute → evaluate
expl graph show                 # ★ marks steps that satisfy a goal
expl candidate ls               # choose; then: git merge expl/node-<id>
```
