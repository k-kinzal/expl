# expl

expl explores a problem space as a graph of agent-realised git branches.

An **explorer** proposes steps toward your goals (and, per goal, backward from them), a **planner** draws paths and sets priorities, **executors** realise steps as commits on their own branches in parallel git worktrees, and an **evaluator** judges completed steps against the goals. The graph lives in SQLite under `.expl/`. Exploration never ends by itself: expl generates candidates, and choosing one is yours.

expl is the successor of [k-kinzal/iter](https://github.com/k-kinzal/iter). See [AGENTS.md](AGENTS.md) for the model and architecture.

## Install

```sh
cargo install --path expl_cli
```

Requires `git` and at least one agent CLI (`claude` or `codex`).

## Quick start

```sh
cd your-repo                      # needs at least one commit
expl init                         # writes expl.toml, creates .expl/, origin = HEAD
expl goal add "Fast startup" --objective "cold start under 100ms, measured by bench/startup.sh"
expl run --cycles 3
expl graph show
expl candidate ls
git merge expl/node-7             # adopt the candidate you choose
```

## Commands

| Command | Purpose |
| --- | --- |
| `expl init [--base REV]` | Initialise; the origin node starts at `REV` |
| `expl goal add TITLE [--objective TEXT] [--counter]` | Add a goal; `--counter` also explores backward from it |
| `expl goal ls` / `expl goal prune ID` | List / retire goals |
| `expl node ls [--kind K] [--status S]` | List nodes |
| `expl node inspect ID` (`expl inspect ID`) | Node with edges, evaluations, agent runs |
| `expl node add TITLE [--after ID]… [--before ID]… [--priority N]` | Propose a step by hand |
| `expl node retry ID` / `expl node prune ID` | Re-plan a failed step / discard a step |
| `expl graph show [--format text\|mermaid\|json]` | Render the graph |
| `expl path ls` | The latest plan's paths |
| `expl candidate ls` | Steps evaluated as satisfying a goal |
| `expl logs ID` | Newest raw agent log for a node |
| `expl explore [--from ID] [--direction forward\|backward]` | Run the explorer once |
| `expl plan` | Run the planner once |
| `expl execute [ID]` | Run the executor on one ready step |
| `expl evaluate [ID]` | Run the evaluator |
| `expl run [--cycles N]` | Repeat explore → plan → execute → evaluate |

Listings accept `-q` (ids only) and `--format json` (NDJSON). Data goes to stdout, status to stderr. Exit codes: `0` ok, `1` user input, `2` runtime, `64` configuration.

## Configuration

`expl.toml` (written by `expl init`) declares agents, binds them to roles, and sets run limits:

```toml
[agent.claude]
kind = "claude_code"            # claude_code | codex | generic
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]

[role.executor]
agent = "claude"
instructions = "Run the test suite before finishing."

[run]
parallelism = 2                 # concurrent executions/evaluations
breadth = 3                     # max proposals per exploration
max_pending = 6                 # pause exploration while this many steps wait
```

Agents run unattended in expl-owned worktrees under `.expl/worktrees/`; the default lets Claude Code edit and run commands without prompting. A `generic` agent is any command that reads the prompt on stdin and prints a JSON object; `EXPL_ROLE`, `EXPL_NODE`, and `EXPL_RESPONSE_SCHEMA` are set for it.

## License

Apache-2.0 OR MIT
