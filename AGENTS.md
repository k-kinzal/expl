# AGENTS

## Vision

Expl is an agentic tool for exploring a problem space. As the successor to [k-kinzal/iter](https://github.com/k-kinzal/iter), it searches broadly, deeply, and in parallel—rather than committing to a single linear path—to uncover the strongest ways to achieve a goal, or the answer to a question.

Real problems rarely have a single answer. Expl models exploration as a directed graph of candidate steps toward human-set goals, and drives it through one loop:

1. **Enumerate** the possibilities a goal opens up, broadly and deeply.
2. **Choose** the most promising paths through them.
3. **Try** them, most promising first.
4. **Judge** whether a path satisfies the goal.
5. **Keep going**: unless a human stops it, look at the other paths too.

- **Multiple solutions and goals.** Depending on a goal's level of abstraction, several distinct approaches may satisfy it, and multiple goals can be pursued side by side.
- **Branching and merging.** Competing ideas diverge along separate paths, while complementary steps can merge (`A -> C` and `B -> C`) into a single downstream step.
- **Forward and backward search.** Exploration proposes steps forward from what already exists toward a goal and, when enabled for a goal, can also work backward via *counter-exploration* to identify what must hold just before the goal is met.
- **Wide frame.** Most exploration goes where a goal looks most promising, but some of it deliberately looks elsewhere: niche directions inside the goal's frame that nobody asked for, and counterexamples—things that would work against the goal—so the map also shows what not to do. Exploration enumerates; planning ranks. Unlikely or counterproductive candidates are ranked low rather than discarded, and only duplicates are dropped. A starting point is never exhausted by one exploration, so expl returns to known nodes and keeps growing new candidates from them.

Steps come in two kinds, and a graph can hold both:

- **Change steps** are realized as concrete code: each one is committed on its own Git branch, and the work converges on the **trunk** when a human adopts it.
- **Investigation steps** change nothing. They answer a question about the code or the world—"is this repository affected by CVE-XXXX-YYYY?", "does any caller rely on this behaviour?"—and record a **finding**: the answer (yes, no, or unknown) with its evidence. They produce no commit and never reach the trunk; their findings converge on the **goal** they serve. An investigation goal is answered when evaluation judges that the findings along a path settle it.

Expl drives this graph through a continuous cycle of **exploration**, **planning**, **execution**, and **evaluation**. Exploration proposes candidate steps, planning turns them into prioritized paths and joins, execution carries out ready steps (committing change steps on Git branches, recording the findings of investigation steps), and evaluation judges finished steps and paths against active goals, feeding every outcome back into further exploration.

Finding one passing path never ends the search, because better, simpler, or contradicting alternatives may still exist. Expl's job is to keep exploring and generating verified candidates and findings; the human's job is to define the goals, add candidates of their own whenever they see one (from the CLI, like any other node), and decide which candidates to adopt.

Expl follows the **trunk**, the default branch declared in `expl.toml`. Humans adopt change candidates by merging them into it with git, resolving conflicts as with any merge. Exploration always starts again from where the trunk stands. Earlier positions are never discarded: every origin the trunk has held stays in the graph together with what was adopted into it, so the history of choices stays visible. Explorations left behind on an older origin remain candidates that can still be taken in later.

## Core Modeling

Humans and AI agents collaborate in Expl through one shared mental model anchored in a Git repository. Both sides read and extend the same **graph** of possible steps, with **events** turning human intent and agent outcomes into asynchronous work:

- **Graph** is the shared map of the exploration. It holds **origins** (positions of the trunk; the newest, *active* one is where exploration starts), **steps** (units of work: *change* steps or *investigation* steps), and **goals** (objectives or questions set by a human, each a separate perspective the work is judged from). Directed edges record which nodes build on others, as well as which goals a step heads toward, so paths can branch and join. Humans can add any node from the CLI, not only goals.
- **Trunk** is the default branch (`trunk` in `expl.toml`). Only humans move it, and they do so with git. Expl never writes to it; it follows it. Every running process watches the trunk's tip, and when the tip moves it records a new origin (`origin.advanced`) that builds on the previous origin and on the steps the trunk took in. The previous origin becomes *superseded*, and that chain is the **lineage**. Git is the ground truth for **adoption**: a completed change step is adopted when the trunk contains its commits, either by history (merge or fast-forward) or by patch (squash merge or cherry-pick, detected with `git cherry`). Steps that have not started yet move onto the new origin, while realized steps keep their base. Investigation steps are never adopted; what they found stays on the node and counts toward their goal.
- **Workspace** is the isolated Git worktree where an agent runs. Exploration, planning, and evaluation run in temporary scratch worktrees at the relevant commit. Execution carries out each change step in its own worktree and commits the result to a dedicated branch; when paths join, the new step starts by merging its predecessors' branches. An investigation step runs in a scratch worktree at the code it is about and leaves no commit behind.
- **Event** is an immutable record of something that happened in the graph, such as `goal.added` or `node.ready`. Human actions and agent reactions both advance the system by recording events, and each agent keeps a durable **inbox** for the events its role handles.
- **Agent** is a named declaration in `expl.toml` bound to one **Role**, giving each AI participant a clear, focused contract. One role can be carried out by several agents with different perspectives (for example a mainstream and a fringe explorer), between which the role's events are routed, and each agent can be carried out by several AI CLIs, between which its invocations are routed:
  - **Exploration** proposes new candidate steps forward from realized nodes (starting at the active origin, again whenever the trunk advances, and again from known nodes picked at random) or backward from counter-exploration goals.
  - **Planning** arranges steps into paths, assigns priorities, joins converging paths, ranks unlikely or counterproductive steps low, and prunes duplicates.
  - **Execution** carries out ready steps in their own workspaces: commits for change steps, findings for investigation steps.
  - **Evaluation** checks completed steps and their paths against active goals and records verdicts. It also judges each new origin, so the lineage shows how every adoption moved every goal.
- **Process** is a running instance of an agent. It claims and handles one event at a time from its agent's inbox. Running multiple **replicas** lets independent events run in parallel while keeping processes of the same agent from colliding on the same node.

Together, these primitives close the loop between human intent and AI autonomy: humans set **goals**, add candidates, and merge **candidates** (change steps evaluated as satisfying a goal) into the trunk or read the **findings** that answer a goal, while agents react to **events**, work in isolated **workspaces**, and feed every evaluation back into further exploration.

## Relations

- [k-kinzal/iter](https://github.com/k-kinzal/iter)
