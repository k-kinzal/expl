# expl

expl explores a problem space as a graph of hypotheses, which agents verify as git branches or as findings.

Graph changes produce **events**. A goal command, a process following the trunk, or a handler that records a result can all produce events. Each **consumer** in `expl.toml` subscribes to specific events and declares how to handle them. The default configuration forms this cycle:

| Events | Handler | Result |
| --- | --- | --- |
| `goal.added`, `node.evaluated`, `node.failed`, `origin.advanced`, `node.revisited` | `exploration` | Propose hypotheses, including backward searches from counter-exploration goals |
| `node.proposed`, `node.evaluated`, `node.failed`, `goal.retired`, `origin.advanced` | `planning` | Assemble paths and set priorities, making eligible hypotheses ready |
| `node.ready` | `verification` | Test the highest-priority ready hypothesis in a private clone and record a commit or finding |
| `node.completed`, `node.failed`, `origin.advanced` | `evaluation` | Record whether a hypothesis held and how far the result moves each goal |

A **worker** is a process that leases an event, runs the handler, and acknowledges the result. Replicas share the same inbox, so independent events can run in parallel. The graph, event log, deliveries, and process records live in SQLite under `.expl/`. Exploration continues until a human stops it.

expl follows the **trunk**, the default branch named by `trunk` in `expl.toml`. To adopt a hypothesis, merge its branch with git, resolving conflicts as usual. Running processes notice that the trunk moved, record its new position as the **origin**, and start exploring from there. A step counts as adopted when the trunk contains its commits, whether by merge, fast-forward, squash merge, or cherry-pick. The previous origins stay in the graph with what was adopted into each, so you can always see what was chosen and when, and still adopt a hypothesis that was left behind on an older origin.

expl is the successor of [k-kinzal/iter](https://github.com/k-kinzal/iter). See [AGENTS.md](AGENTS.md) for the model and architecture.

## Install

```sh
cargo install --path expl_cli
cargo install --path expl_ui      # optional: the `expl-ui` window (macOS/Linux, GPUI)
```

Requires `git` and the commands configured for the consumers. The default uses `claude`.

## Quick start

```sh
cd your-repo                      # needs at least one commit
expl init                         # writes expl.toml, creates .expl/, origin = HEAD, trunk = current branch
expl up                           # start the configured workers
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
| `expl event ls [--kind K]… [--consumer NAME]` | The event log and optional delivery status for a consumer |
| `expl consumer ls` | Subscriptions, handlers, concurrency limits, running workers, backlog |
| `expl run CONSUMER [-d] [--name N] [--once] [--exit-when-idle]` | Start a consumer worker (foreground, or detached with `-d`) |
| `expl up [CONSUMER…]` | Start workers until each assigned consumer has `replicas` running |
| `expl down [-t SECS]` | Stop every running process |
| `expl ps [-a]` (`expl process ls`) | List processes |
| `expl logs PROC [-f]` | Print (or follow) a process log |
| `expl stop PROC… [-t SECS]` / `expl kill PROC…` / `expl rm PROC… [-f]` | Stop gracefully / kill / remove records |
| `expl process inspect PROC` | One process record |

Processes are addressed by id or name (`<consumer>-<id>` unless named with `--name`). Listings accept `-q` (ids only) and `--format json` (NDJSON). Data goes to stdout and status goes to stderr. Exit codes: `0` ok, `1` user input, `2` runtime, `64` configuration.

## Watching

`expl-ui` draws the graph live. The trunk is a straight spine with older origins on the left, and the current origin is the bright, ringed dot that exploration grows from. Adopted steps are square, and steps left behind on an older origin fade unless they still satisfy a goal. The top-right panel lists every goal with the trunk's score and the best hypothesis not yet adopted; selecting a goal focuses the graph on that perspective. The bottom-right panel lists the trunk's history, newest first: what each origin adopted.

## Configuration

`expl init` writes `expl.toml` with the default cycle. Each consumer declares its event kinds in `on`, its graph operation in `handler`, and the command that supplies the handler's response. For example, these subscriptions run when a goal is added and when a step is ready, respectively.

```toml
trunk = "main"

[consumer.explorer]
on = ["goal.added"]
handler = "exploration"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1
breadth = 3

[consumer.verifier]
on = ["node.ready"]
handler = "verification"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 2
concurrency = 2
instructions = "Run the test suite before finishing."
```

The handler prepares the workspace and the request, validates the response, and records the result in the graph. Drivers include `claude_code`, `codex`, `cursor_agent`, and `generic`. Each `[[consumer.<name>.alternative]]` adds another command to the invocation rotation and failover. `timeout` bounds each attempt.

A `generic` command reads the handler's prompt on standard input and returns JSON on standard output. It receives `EXPL_CONSUMER`, `EXPL_HANDLER`, `EXPL_EVENT`, `EXPL_EVENT_ID` (the numeric log ID), `EXPL_NODE` when there is a subject, and `EXPL_RESPONSE_SCHEMA`. A command can use an algorithm or delegate to a service and return the same response schema. The subscription, retries, and graph updates belong to expl.

Commands run in private clones with the full Git history. `[workspace] frozen` lists Git pathspecs a change step's commit keeps as they were where the step started. An exploration consumer that subscribes to `node.revisited` can set `revisit_cooldown` to publish that event after an idle interval, choosing a starting point at random.

### Routing and concurrency

Each consumer receives all events it subscribes to by default. For interchangeable consumers, give them the same `group` and point that group at a consumer or router in `[routing]`. Each event has one recipient per group. Consumers with independent work, such as exploration and evaluation on `origin.advanced`, can use separate groups or inboxes.

For example, set `group = "directions"` on `explorer` and `fringe`, and `group = "checks"` on `verifier`. The following assigns one exploration perspective per subscribed event and limits all three consumers to one simultaneous reaction.

```toml
[routing]
directions = "project"
checks = "project"

[router.project]
targets = ["perspectives", "verifier"]
policy = "first"
concurrency = 1

[router.perspectives]
targets = ["explorer", "fringe"]
policy = "weighted"
weights = { explorer = 4, fringe = 1 }
concurrency = 1
```

A router considers only members of the group that subscribe to the event. `first` picks the first eligible target. `weighted` splits events in proportion to the target weights, which default to one. Expl records the recipient for each group and event, so retries and restarts keep it while it remains eligible.

`concurrency` limits simultaneous reactions across all workers of a consumer or all assigned descendants of a router. `replicas` controls how many workers `expl up` starts. Workers started with `expl run` share the same limits. Restart workers after editing their configuration.

### Existing projects

Existing `[agent.<name>]` declarations still load with the former event sets and routing. To migrate, rename each table to `[consumer.<name>]`, replace `role` with `handler`, and add the corresponding `on` list from the default cycle above. For explicit routing, keep the existing `[routing]` keys and set `group` to the former role on each routed consumer. Convert inline `weight` settings to a weighted router and use the former role as the group name. Keep consumer names to preserve their delivery history. The CLI also accepts `expl agent ls` and `expl event ls --agent` for existing scripts.

## License

Apache-2.0 OR MIT
