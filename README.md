# expl

expl explores a problem space as a graph of hypotheses, which agents verify as git branches or as findings.

Graph changes produce **events**. A goal command, a process following the trunk, or a handler that records a result can all produce events. Each **consumer** in `expl.toml` subscribes to specific events and declares how to handle them. The default configuration forms this cycle:

| Events | Handler | Result |
| --- | --- | --- |
| `goal.added`, `node.evaluated`, `node.failed`, `origin.advanced`, `node.revisited` | `exploration` | Propose hypotheses, including backward searches from counter-exploration goals |
| `node.proposed`, `node.evaluated`, `node.failed`, `goal.retired`, `origin.advanced` | `planning` | Assemble paths and set priorities, making eligible hypotheses ready |
| `node.ready` | `verification` | Test the highest-priority ready hypothesis in a private clone and record a commit or finding |
| `node.completed`, `node.failed`, `origin.advanced` | `evaluation` | Record whether a hypothesis held and how far the result moves each goal |

A **worker** is a process that leases an event, executes the consumer's reaction as a command, and settles the delivery from its outcome. Replicas share the same inbox, so independent events can run in parallel. The graph, event log, deliveries, and process records live in SQLite under `.expl/`. Exploration continues until a human stops it.

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

## Running a daemon

Use `expl daemon` in an initialized checkout to run a standalone daemon. A single daemon elects itself and also executes commands. In another terminal, use the same `expl up`, `expl goal add`, `expl logs`, and other commands. An unmanaged checkout can still run the quick start directly.

For multiple machines, put the checkout and a coordination directory on shared storage and mount both at the **same absolute paths** on all machines. Use one initialized checkout and stop any existing workers before starting the first daemon. Each machine needs Git, a compatible `expl` binary, and the configured agent commands and credentials. Run the daemons under the same service account on storage that provides cross-host file locking, coherent reads, atomic renames, and durable SQLite rollback journaling. See [SQLite's network filesystem requirements](https://sqlite.org/useovernet.html).

```sh
# On machine A
expl -C /shared/project --cluster /shared/expl-cluster --member a daemon

# On machine B
expl -C /shared/project --cluster /shared/expl-cluster --member b daemon

# From either machine, through member b
export EXPL_MEMBER=b
expl --cluster /shared/expl-cluster cluster status
expl --cluster /shared/expl-cluster up
expl --cluster /shared/expl-cluster ps
```

The checkout remembers its daemon directory, and `EXPL_MEMBER` selects the local daemon for both `expl` and `expl-ui`. The leader owns the graph, deliveries, routing, Git publication, and worker records. Members forward operations to the leader and run commands in their assigned private clones. The leader distributes commands by the number already queued or running on each member. Use the existing `replicas` and `concurrency` settings to increase parallelism.

The shared election lock admits one leader. If it exits, a surviving member takes over after the previous leader's application processes release their guards. It recovers the deliveries and restarts the detached workers that were still requested. An interrupted CLI operation can be inspected through the new leader. Stop workers with `expl down` before shutting down the daemons if the workers should stay stopped.

### Several explorers on one daemon

A daemon hosts every explorer registered with it. `expl daemon` inside a project registers that project. To host more, start the daemon on a directory of its own and let projects join it:

```sh
expl --cluster /shared/expl-cluster daemon             # hosts whatever is registered
expl -C project-a --cluster /shared/expl-cluster join  # the project registers itself
expl -C project-a up                                   # its workers start under the daemon
expl --cluster /shared/expl-cluster cluster status     # leader, explorers, agent limits in use
```

A command typed inside a project is about that project. Elsewhere, name the explorer with `--explorer NAME` (or `EXPL_EXPLORER`). A project registers under its directory name, so two projects with the same directory name cannot join one daemon.

An explorer that is not an expl project registers from a file and is managed with `expl explorer`. See [Building a specialized explorer](#building-a-specialized-explorer).

### Agent control

The daemon's operator controls the agents of every explorer it hosts, in `daemon.toml` in the daemon directory. Without the file there are no limits.

```toml
[agents]
concurrency = 2                 # agent invocations at once, across every explorer

[pool.specialized]
agents = 1                      # across the explorers that register pool = "specialized"

[agent.claude]                  # a named agent, for consumers that say agents = ["claude"]
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
timeout = "30m"
```

An agent invocation takes one slot of the daemon and one of its explorer's pool before it starts, and waits while none is free. The limits are read again for each invocation, so editing the file needs no restart. While the file cannot be read (a typo, say), invocations wait and events are given back uncounted; nothing fails for it. An explorer's pool is the `pool` it registers (top-level `pool = "…"` in `expl.toml`), or one named after the explorer. A consumer in `expl.toml` can name the daemon's agents with `agents = ["claude", "cursor"]` in place of `driver`, `command` and `args`. Its own `env`, `timeout` and alternatives still apply. `expl cluster status` shows the limits and the slots in use.

### Git remote

In a managed or unmanaged checkout, set `remote = "origin"` under `[workspace]` to follow the remote trunk and publish change-step branches to that remote. The leader fetches the trunk into a private tracking ref, so the local checkout can stay where it is. Agents still work in isolated clones. Adopt a branch from your own checkout with Git, then push the updated trunk. The configured Git remote must allow the daemon's account to fetch the trunk and create branch refs.

```toml
[workspace]
remote = "origin"
frozen = []
```

## Building a specialized explorer

[`expl_runtime`](expl_runtime/src/lib.rs) is the framework. [`expl_core`](expl_core/src/lib.rs) is the general-purpose explorer, one application of it. A specialized explorer is another application of the same framework. It has no dependency on expl's graph, goal types, or four handlers, and expl has none on it.

The framework owns delivery. An explorer keeps its state and its events in one SQLite database, registers its consumers, and supplies the command executed for one event. The daemon's workers do the rest: lease the next event, execute the command, keep the lease alive, settle the delivery, retry or give up.

1. **Store and publish.** [`store::open`](expl_runtime/src/store.rs) opens the database and `store::initialize` creates the framework's tables in it. Add your own tables beside them. [`event::EventLog::publish`](expl_runtime/src/event.rs) appends an event with a kind, an optional subject and payload. Publish in the same transaction as the change that produced the event. `EventLog::prioritize` ranks an event: delivery is by priority, then age.
2. **Register.** Describe the explorer in a file and add it to a daemon.

   ```toml
   name = "prover"
   database = ".prover/prover.db"
   pool = "specialized"                 # whose agent limit its work counts against

   [consumer.checker]
   on = ["proof.proposed"]
   run = ["prover", "check"]            # executed for ONE event
   placement = "any"                    # on any member; "leader" (default) beside the database
   replicas = 1
   concurrency = 1

   [task.sweep]                         # a command run every 60 seconds
   every = 60
   run = ["prover", "sweep"]
   ```

   ```sh
   expl --cluster DIR explorer add prover.toml
   expl --cluster DIR explorer up prover
   expl --cluster DIR explorer ps prover
   ```

   Consumers can share events through `group`, `[routing]` and `[router.<name>]` exactly as in `expl.toml`. `exclusive = true` allows one reaction at a time whatever the number of workers. `coalesce = true` lets one reaction cover every earlier event of the subscription. `idle = { after = 600, run = [...] }` runs a command when the consumer has had nothing to react to for that long.
3. **React.** The command runs in the explorer's directory, with the path of the explorer's database in `EXPL_RUNTIME_DATABASE` (`daemon::bound_database()`). It finds the delivery in its environment (see [`reaction`](expl_runtime/src/reaction.rs)): `EXPL_CONSUMER`, `EXPL_EVENT_ID`, `EXPL_EVENT`, `EXPL_SUBJECT`, `EXPL_ATTEMPTS`, `EXPL_MAX_ATTEMPTS`, `EXPL_LEASE`, and `EXPL_REACTION` (`react`, or `abandon` to clean up after a holder that died on the last attempt). Exiting with success acknowledges the event. Failing asks for it again, up to three attempts; a command that could not be started at all is not an attempt. For more, write an outcome to the file named by `EXPL_OUTCOME`: acknowledge with a note, release without counting the attempt, or retry with the reason. In `abandon` mode, release means the cleanup could not be made yet, and the worker asks again later. A Rust command reads all of this with `reaction::Reaction::from_env()` and answers with `Reaction::report`.
4. **Use agents and clones.** [`agent::Agent`](expl_runtime/src/agent/mod.rs) runs a prompt and a response schema through an agent CLI. Call [`daemon::govern`](expl_runtime/src/daemon.rs) on it first, so the invocation runs on a member and under the daemon's agent limits. [`git::AgentClone`](expl_runtime/src/git/agent_clone.rs) provides isolated Git workspaces with restoration and file snapshots.

For example, a mathematical explorer can publish a `literature.wanted` event, register a consumer whose command searches and stores the references in its own tables, and publish `proof.proposed` for a consumer that runs a proof check in the relevant Git workspace. Each command decides how its result changes the search and which event to publish next.

`expl explorer publish NAME KIND [--subject N] [--payload JSON]` appends an event from the command line, which is enough to drive an explorer whose commands are shell scripts.

## Commands

| Command | Purpose |
| --- | --- |
| `expl daemon` / `expl cluster status` | Serve the registered explorers as leader or member / show the selected member, the leader, the explorers, and the agent limits in use |
| `expl join` | Register this project with the daemon named by `--cluster` |
| `expl explorer ls` / `add FILE` / `rm NAME` | List the daemon's explorers / register one from a file / remove one |
| `expl explorer up NAME [CONSUMER…]` / `down NAME` / `ps NAME` | Start, stop, or list a registered explorer's workers |
| `expl explorer publish NAME KIND [--subject N] [--payload JSON]` | Append an event to a registered explorer's log |
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

### OpenTelemetry

Set an OTLP HTTP base URL in `expl.toml` to export traces and metrics. This repository sends its exploration telemetry to the GPU server.

```toml
[telemetry]
endpoint = "http://192.168.10.4:4318"
```

Expl uses OTLP over HTTP with Protobuf and appends `/v1/traces` or `/v1/metrics` to the base URL. `OTEL_EXPORTER_OTLP_ENDPOINT` overrides the project URL. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` take full URLs and override the base for their respective signals. The SDK also reads the standard OTLP headers and timeouts, `OTEL_SERVICE_NAME` (default `expl`), and `OTEL_RESOURCE_ATTRIBUTES`. Use `OTEL_SDK_DISABLED=true` to turn off all export, or set the respective `OTEL_TRACES_EXPORTER` or `OTEL_METRICS_EXPORTER` to `none`.

- Filter delivery traces by `expl.consumer`, `expl.handler`, `expl.event.kind`, `expl.event.id`, or `expl.node.id`. A traced producer leaves a link on its event that survives worker restarts. Child spans show the search direction and target goals, workspace operations, and agent attempts. Delivery results include proposal IDs, plan changes, change commits or investigation answers, and evaluation decisions and scores.
- Use `expl.worker.state` to see workers waiting for events, waiting for capacity, or reacting. `expl.delivery.count` counts delivery outcomes, including retries, lost leases, and failed verifications. `expl.delivery.duration` measures time from lease to settlement, and `expl.delivery.queue_age` measures event age at lease. Both durations are in seconds. Workers export metrics every 15 seconds by default. Set `OTEL_METRIC_EXPORT_INTERVAL` in milliseconds to change this.
- Process logs include `trace_id` and `span_id` for sampled reactions. Prompts, agent output, and finding evidence stay in the local logs and graph.

In Tempo, search for `{ resource.service.name = "expl" && span.expl.handler = "verification" }`. In Prometheus, `expl_worker_state{expl_worker_phase="reacting"} == 1` shows the workers currently handling a delivery.

### Existing projects

Existing `[agent.<name>]` declarations still load with the former event sets and routing. To migrate, rename each table to `[consumer.<name>]`, replace `role` with `handler`, and add the corresponding `on` list from the default cycle above. For explicit routing, keep the existing `[routing]` keys and set `group` to the former role on each routed consumer. Convert inline `weight` settings to a weighted router and use the former role as the group name. Keep consumer names to preserve their delivery history. The CLI also accepts `expl agent ls` and `expl event ls --agent` for existing scripts.

## License

Apache-2.0 OR MIT
