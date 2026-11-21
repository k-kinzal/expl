---
name: expl-adopt
description: Review an expl candidate (a step branch expl/node-<id>) and merge it into main — the Markdown freeze, the build/test gate, choosing among competing candidates, and what to do after the merge. Use whenever `expl candidate ls` lists something, or a completed step looks worth taking in.
---

# Adopting candidates into main

Adoption is a plain git merge into the trunk (`main`). Running processes notice the
move within seconds, record a new origin (`origin.advanced`), evaluate it against
every goal, and explore again from there. Nothing else needs to be told.

## Gate (all must hold, no exceptions)

1. **No Markdown change.** `git diff --name-only $(git merge-base main expl/node-N) expl/node-N -- '*.md'`
   prints nothing. A branch touching any `.md` is never merged — prune it or let it
   be; do not "fix it up" by reverting the file yourself and merging.
2. **Merges cleanly into the current `main`.**
3. **On the merged tree:** `cargo fmt --all --check`, `cargo clippy --all-targets`,
   `cargo test` pass.
4. **It is a real improvement toward a goal:** read the evaluator's reasons
   (`expl node inspect N` → `evaluations`) and skim the diff. Reject steps that
   delete tests, weaken lints (`allow(...)` sprinkled to silence clippy), or stub
   behaviour.

`skills/expl-adopt/scripts/check-candidate.sh N` performs 1–3 in a throwaway
worktree (the main checkout is untouched) and exits non-zero with the reason.

## Procedure

```sh
expl candidate ls                                   # best first; BEHIND = commits main moved since
expl node inspect N | less                          # evaluations, reasons, predecessors
git log --oneline main..expl/node-N; git diff main...expl/node-N --stat
skills/expl-adopt/scripts/check-candidate.sh N      # the gate
skills/expl-adopt/scripts/adopt.sh N                # gate again + merge --no-ff + cargo install
expl origin ls                                      # confirm: new origin, ADOPTED shows #N
```

`adopt.sh` refuses unless you are on a clean `main`. It installs the adopted expl
(`cargo install --path expl_cli`) so newly started processes run it. Pass
`--restart` (`adopt.sh --restart N`) to restart all processes right away — do this
when the adoption changes the database schema, the inbox/process/graph code the
running processes depend on, or fixes a bug they suffer from. Otherwise prefer to
restart when executors are idle (`expl ps`), because a restart interrupts and
redoes in-flight executions.

Also not a candidate yet but completed and high-scoring? Evaluation lists only
`satisfied` verdicts as candidates. A completed step with a solid partial score
that passes the gate may be adopted too when it is clearly an increment toward the
vision — the vision goal is large and will rarely be "satisfied" by one step.
Prefer adopting small verified increments over waiting for a single perfect branch.

## Choosing among competitors

- Several steps solving the same thing: take the simplest that passes the gate; the
  others stay in the graph as alternatives (they will show as BEHIND).
- Steps on an older origin can still be adopted; if they conflict, do not resolve
  conflicts by hand in non-trivial code — let expl re-explore from the new origin.
- After an adoption, re-run `check-candidate.sh` for the next one: the gate is
  always against the current `main`.

## After merging

- `expl origin ls` should show the new origin with `#N` adopted within a few seconds.
  If it does not, see `expl-troubleshoot` ("trunk not followed").
- Tell the owner briefly what was adopted and why (one line per adoption).
- Never push, force-push, rebase or rewrite `main`; the lineage depends on its history.
