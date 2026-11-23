---
name: expl-troubleshoot
description: Diagnose and repair a running expl that stopped making progress — failed deliveries, dead or stuck processes, agent CLI failures, worktree/branch trouble, trunk not followed — and the rules for fixing expl's own code so exploration keeps going. Use when `expl ps`/`expl event ls` show failures, nothing moves, or a CLI errors.
---

# Troubleshooting expl

Fixing expl is part of the controller's job **only to keep exploration going**.
Feature work belongs to expl's own agents. A fix must be the smallest change that
removes the blocker, with a test, and it is committed straight to `main` (expl then
follows it like any adoption). Never change `*.md` files while fixing.

## First look

```sh
skills/expl-operate/scripts/status.sh        # everything at once
expl ps -a                                   # exited/dead processes and their reason
expl event ls --consumer <consumer>          # per-delivery state: failed, attempts, note
expl logs <process> | tail -50               # process log: reacting / reacted / failed lines
expl node logs <id>                          # raw agent output of the newest run about a node
ls -t .expl/logs | head                      # newest agent runs
```

## Symptom → cause → action

| Symptom | Likely cause | Action |
| --- | --- | --- |
| `failed: … (will be delivered again)` then `(giving up)` in a process log | the agent CLI failed 3× on that event (`MAX_ATTEMPTS`) | read the run log; fix the cause (config, CLI auth, bug); for a failed step use `expl node retry <id>` |
| run log ends with `claude reported error_*` / `usage limit` / codex exit 1 | CLI quota or outage | routing falls over to the other CLI automatically; if both fail, wait, or temporarily remove the broken CLI from `expl.toml` and restart that consumer’s workers |
| `codex's last message is not a JSON object` / `no structured output` | CLI answered prose | usually transient (retried); if systematic, check the handler schema still matches what the CLI accepts |
| cursor-agent `is_error` / auth error | Cursor login expired | ask the owner to run `! cursor-agent login`; then `expl node retry <id>` for failed steps |
| step `failed` with "cargo … failed" in its summary | verifier could not make tests pass | normal outcome; planning re-plans or prunes it — leave it unless it repeats for the same reason |
| process `dead` in `expl ps` | crashed or killed without exiting | `expl rm <name>` then `expl up`; its leases expire after 60 s and the events are redelivered |
| a step stuck `running` with no live verifier | verifier died mid-run | the lease expires and the redelivered `node.ready` reclaims it; if not, `expl stop`/`up` the verifiers |
| nothing `ready` although steps are `planned` | predecessors not completed, or planner never prioritised them | `expl path ls`, `expl event ls --consumer planner` |
| merged into main but `expl origin ls` shows no new origin | no process is running, or trunk branch mismatch | `expl up`; `trunk = "main"` in `expl.toml`; the process log shows `cannot follow the trunk: …` |
| `worktree add` / `branch -D` errors | leftover worktree or branch from a crash | `git worktree prune`; inspect `.expl/worktrees`; never delete `expl/node-*` branches of completed steps |
| `expl` prints nothing and exits (even `--version`) | the binary was overwritten in place with `cp`; macOS kills it (invalid code signature) | reinstall with `skills/expl-operate/scripts/install.sh` (copy + rename) |
| stray `.../cargo-target/debug/expl -C /var/folders/.../T/.tmp*/repo run …` processes | test sandboxes whose processes outlived an interrupted `cargo test` | kill them: `pkill -f 'cargo-target/debug/expl -C /private/var/folders'` |
| process exits with `database is locked` (code 5 or 517) | a write raced another process (517) or waited out the busy timeout under heavy load (5) | fixed in 3c8d3ad: writes are `BEGIN IMMEDIATE` and a busy reaction gives its event back; if it recurs, `expl up` and look for a process holding a long write |
| builds look hung | cargo waits on the shared target dir lock | expected with several agents; check `ps aux | grep cargo` before suspecting a hang |

## Fixing expl's code

1. Reproduce from the logs; write down the exact error line.
2. Work in the main checkout on `main` (or a scratch worktree), make the minimal fix,
   add or extend a test under `expl_core`/`expl_cli` that fails without it.
3. Gate: `cargo fmt --all --check && cargo clippy --all-targets && cargo test`.
4. Commit to `main` with a `fix:` message (with the Co-Authored-By trailer).
5. `skills/expl-operate/scripts/install.sh` (`--ui` too if the UI changed). Never
   `cargo install` (rebuilds gpui from scratch, 20+ minutes) and never plain `cp`
   over the binary (macOS then kills every launch: invalid code signature). Then
   `skills/expl-operate/scripts/rolling-restart.sh` so processes run the fix.
   Restart `expl-ui` if it was rebuilt.
6. Retry what the bug broke (`expl node retry <id>`), and watch one full cycle.

The DB schema lives in `expl_core/src/store.rs`. Never hand-edit `.expl/expl.db`
while processes run; if state must be repaired, `expl down` first and keep a copy
(`cp .expl/expl.db .expl/operator/expl.db.bak-$(date +%s)`).

## Known history of fixes (append as you go)

- Agents could only use one CLI, and Cursor Agent had no driver. Added the
  `cursor_agent` driver and `[[agent.<name>.alternative]]` routing (rotate + failover).
- Exploration only reacted to events, so a starting point was explored once and
  then never again. Added `node.revisited`: an idle explorer revisits a random
  starting point after `revisit_cooldown` seconds, and exploration prompts list the
  existing steps (section "Already proposed") so repeats grow new alternatives.
- Exploration was narrow (`breadth = 3`, and the prompt told agents to drop weak
  ideas), and several agents of one role all reacted to every event. Exploration now
  enumerates broadly, planning ranks risky steps low instead of pruning them, and
  `weight` routes a role's events between agents (mainstream `explorer` / `fringe`).
- Joins whose merge conflicted always failed: the agent resolved the markers but git
  still listed the files as unmerged. Fixed by adopting #3 (`Workspace::resume`
  concludes the merge after checking the markers are gone).
- A newly declared consumer sees past events it subscribes to. For a consumer in
  a delivery group, existing assignments keep their recipients while eligible.
- Processes died with `database is locked`: deferred savepoints that read before
  writing failed at once with `SQLITE_BUSY_SNAPSHOT` when another process had
  committed. Writes now take the lock up front, and a reaction that still finds the
  database busy hands its event back and continues.
- `install.sh` built the working tree, so an install while editing shipped
  uncommitted code. It now refuses when `*.rs`/`*.toml`/`Cargo.lock` are dirty —
  develop larger changes in a separate worktree (`git worktree add … -b work/…`).
