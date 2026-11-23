# expl

expl explores a problem space as a graph of hypotheses, which agents verify as git branches or as findings.

Every change to the graph is an **event**. Agents declared in `expl.toml` react to the events of their role:

- **exploration** proposes hypotheses — steps toward your goals, and for counter-exploration goals also backward from them;
- **planning** assembles them into paths of hypotheses built on one another and sets their priorities;
- **verification** tests each ready hypothesis, highest priority first: a change becomes a commit on its own branch, an investigation a finding, worked out in a private clone of the repository;
- **evaluation** reads each verification's result: whether the hypothesis held (done, abandoned, or to be continued), and how far it moves each goal.

Agents run as **processes**, managed like Docker containers. Processes of one agent share its events, so independent hypotheses are explored, verified, and evaluated in parallel. The graph, events, and processes live in SQLite under `.expl/`. Exploration never ends by itself: expl generates and verifies hypotheses, and choosing one is up to you.

expl follows the **trunk**, the default branch named by `trunk` in `expl.toml`. To adopt a hypothesis, merge its branch with git, resolving conflicts as usual. Running processes notice that the trunk moved, record its new position as the **origin**, and start exploring from there. A step counts as adopted when the trunk contains its commits, whether by merge, fast-forward, squash merge, or cherry-pick. The previous origins stay in the graph with what was adopted into each, so you can always see what was chosen and when, and still adopt a hypothesis that was left behind on an older origin.

expl is the successor of [k-kinzal/iter](https://github.com/k-kinzal/iter). See [AGENTS.md](AGENTS.md) for the model and architecture.

## Install

```sh
cargo install --path expl_cli
cargo install --path expl_ui      # optional: the `expl-ui` window (macOS/Linux, GPUI)
```

Requires `git` and at least one agent CLI (`claude` or `codex`).

## Quick start

```sh
cd your-repo                      # needs at least one commit
expl init                         # writes expl.toml, creates .expl/, origin = HEAD, trunk = current branch
expl up                           # start every declared agent
expl goal add "Fast startup" --objective "cold start under 100ms, measured by bench/startup.sh"
expl ps                           # what each process is doing
expl-ui &                         # or watch it: the graph, live
expl graph show
expl hypothesis ls
git merge expl/node-7             # adopt the hypothesis you choose; the origin follows the trunk
expl origin ls                    # the lineage: every origin, what it adopted, trunk scores per goal
expl down
```

## Commands

| Command | Purpose |
| --- | --- |
| `expl init [--base REV] [--trunk BRANCH]` | Initialise; the first origin starts at `REV`, and the trunk to follow (default: the current branch) is written to `expl.toml` |
| `expl goal add TITLE [--objective TEXT] [--counter] [--under GOAL]…` | Add a goal (`goal.added`); `--counter` also explores backward from it; `--under` makes it a subgoal of those goals |
| `expl goal nest ID [--under GOAL]…` | Place an existing goal under other goals as their subgoal (none: back to the top level) |
| `expl goal ls [-a]` / `expl goal retire ID` | List goals (`-a`: retired ones too) / retire a goal |
| `expl node ls [--kind K] [--status S]` | List nodes |
| `expl node inspect ID` | Node with edges, evaluations, agent runs |
| `expl node add TITLE [--after ID]… [--before ID]… [--priority N]` | Propose a hypothesis (a step) by hand (`--priority` plans it at once) |
| `expl node retry ID` / `expl node prune ID` | Re-plan a failed step / discard a step |
| `expl node logs ID` | Newest raw agent log about a node |
| `expl graph show [--format text\|mermaid\|json]` | Render the graph |
| `expl path ls` | The latest plan's paths |
| `expl hypothesis ls` | Hypotheses evaluated as satisfying a goal: the origin each was explored from, how far behind it is, and whether it is already adopted |
| `expl origin ls` | The lineage: every origin, oldest first, with what it adopted and how the trunk scored against each goal |
| `expl event ls [--kind K]… [--agent NAME]` | The event log; with `--agent`, how that agent's deliveries stand |
| `expl agent ls` | Declared agents: role, replicas, running processes, backlog |
| `expl run AGENT [-d] [--name N] [--once] [--exit-when-idle]` | Start a process of an agent (foreground, or detached with `-d`) |
| `expl up [AGENT…]` | Start processes until each agent has `replicas` running |
| `expl down [-t SECS]` | Stop every running process |
| `expl ps [-a]` (`expl process ls`) | List processes |
| `expl logs PROC [-f]` | Print (or follow) a process log |
| `expl stop PROC… [-t SECS]` / `expl kill PROC…` / `expl rm PROC… [-f]` | Stop gracefully / kill / remove records |
| `expl process inspect PROC` | One process record |

Processes are addressed by id or name (`<agent>-<id>` unless named with `--name`). Listings accept `-q` (ids only) and `--format json` (NDJSON). Data goes to stdout and status goes to stderr. Exit codes: `0` ok, `1` user input, `2` runtime, `64` configuration.

## Watching

`expl-ui` draws the graph live. The trunk is a straight spine with older origins on the left, and the current origin is the bright, ringed dot that exploration grows from. Adopted steps are square, and steps left behind on an older origin fade unless they still satisfy a goal. The top-right panel lists every goal with the trunk's score and the best hypothesis not yet adopted; selecting a goal focuses the graph on that perspective. The bottom-right panel lists the trunk's history, newest first: what each origin adopted.

## Configuration

`expl.toml` (written by `expl init`) declares the trunk and the agents:

```toml
trunk = "main"                  # the default branch expl follows

[agent.explorer]
role = "exploration"            # exploration | planning | verification | evaluation
driver = "claude_code"          # claude_code | codex | generic
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1                    # processes `expl up` keeps running
breadth = 3                     # exploration only: proposals per reaction
max_pending = 6                 # exploration only: pause while this many steps wait

[agent.verifier]
role = "verification"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 2                    # two hypotheses are verified at once
instructions = "Run the test suite before finishing."
```

You can declare several agents for one role, for example a Claude Code verifier and a Codex verifier. Each one receives every `node.ready`, and whichever process claims a step first verifies it. Agents run unattended in private clones under `.expl/worktrees/`, with the whole history and free use of git; the project's refs move only by expl and by you. `[workspace] frozen` lists git pathspecs a step's commit keeps as they were. The default lets Claude Code edit files and run commands without prompting.

A `generic` agent is any command that reads the prompt on stdin and prints a JSON object. expl sets `EXPL_AGENT`, `EXPL_ROLE`, `EXPL_EVENT`, `EXPL_NODE`, and `EXPL_RESPONSE_SCHEMA` for it.

## License

Apache-2.0 OR MIT
