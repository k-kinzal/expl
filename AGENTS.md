# AGENTS

## Vision

Expl is an agentic tool for exploring a problem space. As the successor to [k-kinzal/iter](https://github.com/k-kinzal/iter), it searches broadly and in parallel—rather than committing to a single linear path—to uncover the strongest ways to achieve a goal.

Real engineering problems rarely have a single answer. Expl models exploration as a directed graph grounded in Git, where every step is realized as concrete code on its own branch:

- **Multiple solutions and goals.** Depending on a goal's level of abstraction, several distinct approaches may satisfy it, and multiple goals can be pursued side by side.
- **Branching and merging.** Competing ideas diverge along separate paths, while complementary steps can merge (`A -> C` and `B -> C`) into a single downstream step.
- **Forward and backward search.** Exploration proposes steps forward from existing code toward a goal and, when enabled for a goal, can also work backward via *counter-exploration* to identify what must hold just before the goal is met.

Expl drives this graph through a continuous cycle of **exploration**, **planning**, **execution**, and **evaluation**. Exploration proposes steps, planning turns them into prioritized paths and joins, execution implements ready steps on Git branches, and evaluation tests finished branches against active goals, feeding every outcome back into further exploration.

Finding one passing branch never ends the search, because better or simpler alternatives may still exist. Expl's job is to keep exploring and generating verified candidates; the human's job is to define the goals and decide which candidates to adopt.

Expl follows the **trunk**, the default branch declared in `expl.toml`. Humans adopt candidates by merging them into it with git, resolving conflicts as with any merge. Exploration always starts again from where the trunk stands. Earlier positions are never discarded: every origin the trunk has held stays in the graph together with what was adopted into it, so the history of choices stays visible. Explorations left behind on an older origin remain candidates that can still be taken in later.

## Core Modeling

Humans and AI agents collaborate in Expl through one shared mental model anchored in a Git repository. Both sides read and extend the same **graph** of possible changes, with **events** turning human intent and agent outcomes into asynchronous work:

- **Graph** is the shared map of the exploration. It holds **origins** (positions of the trunk; the newest, *active* one is where exploration starts), **steps** (units of work), and **goals** (objectives set by a human, each a separate perspective the work is judged from). Directed edges record which nodes build on others, as well as which goals a step heads toward, so paths can branch and join.
- **Trunk** is the default branch (`trunk` in `expl.toml`). Only humans move it, and they do so with git. Expl never writes to it; it follows it. Every running process watches the trunk's tip, and when the tip moves it records a new origin (`origin.advanced`) that builds on the previous origin and on the steps the trunk took in. The previous origin becomes *superseded*, and that chain is the **lineage**. Git is the ground truth for **adoption**: a completed step is adopted when the trunk contains its commits, either by history (merge or fast-forward) or by patch (squash merge or cherry-pick, detected with `git cherry`). Steps that have not started yet move onto the new origin, while realized steps keep their base.
- **Workspace** is the isolated Git worktree where an agent runs. Exploration, planning, and evaluation run in temporary scratch worktrees at the relevant commit, while execution implements each step in its own worktree and commits the result to a dedicated branch. When paths join, the new step starts by merging its predecessors' branches.
- **Event** is an immutable record of something that happened in the graph, such as `goal.added` or `node.ready`. Human actions and agent reactions both advance the system by recording events, and each agent keeps a durable **inbox** for the events its role handles.
- **Agent** is a named declaration in `expl.toml` bound to one **Role**, giving each AI participant a clear, focused contract:
  - **Exploration** proposes new steps forward from realized nodes (starting at the active origin, and again whenever the trunk advances) or backward from counter-exploration goals.
  - **Planning** arranges steps into paths, assigns priorities, joins converging paths, and prunes unpromising ones.
  - **Execution** implements ready steps in their own workspaces.
  - **Evaluation** checks completed steps against active goals and records verdicts. It also judges each new origin, so the lineage shows how every adoption moved every goal.
- **Process** is a running instance of an agent. It claims and handles one event at a time from its agent's inbox. Running multiple **replicas** lets independent events run in parallel while keeping processes of the same agent from colliding on the same node.

Together, these primitives close the loop between human intent and AI autonomy: humans set **goals** and merge **candidates** (steps evaluated as satisfying a goal) into the trunk, while agents react to **events**, work in isolated **workspaces**, and feed every evaluation back into further exploration.

## Relations

- [k-kinzal/iter](https://github.com/k-kinzal/iter)

