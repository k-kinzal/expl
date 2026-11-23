---
name: expl-adopt
description: Decide whether an expl hypothesis (a step branch expl/node-<id>) belongs in main, and merge it — judging what the goals need, reviewing the node's code and the merged whole, the Markdown freeze and build/test gate, and what to do after the merge. Use whenever `expl hypothesis ls` lists something, or a completed step looks worth taking in.
---

# Adopting hypotheses into main

Adoption is a plain git merge into the trunk (`main`). Running processes notice the
move within seconds, record a new origin (`origin.advanced`), evaluate it against
every goal, and explore again from there.

## What adoption is for

Adoption is the one decision expl leaves to whoever controls it, because it cannot
be made mechanically. If "score ≥ X and the gate passes" were enough, expl would do
it itself. **Merge what the goals need**: a change that is a correct, necessary part
of reaching a goal, and that leaves `main` as a whole better placed for every goal.

- **Scores are evidence, not the criterion.** An evaluator judges how far a branch
  gets toward a goal; it does not review the code, and it does not see how the
  branch combines with everything else in `main`. A high score never justifies a
  merge on its own, and a modest one does not rule out a needed piece.
- **The gate is necessary, not sufficient.** A clean merge and green tests say
  nothing about races, lost work, or whether the change is needed at all.
- **The owner's goals come first.** A feature the owner asked for that `main`
  still lacks outranks polishing what exists. Check the goals (`expl goal ls`) and
  the latest origin verdicts (`expl node inspect <origin>`) for what is still
  missing, and prefer hypotheses — or your own work, when nothing in the graph
  gets there — that close those gaps.

## Gate (mechanical; all must hold)

1. **No Markdown change.** A branch touching any `*.md` is never merged — do not
   "fix it up" by reverting the file yourself.
2. **Merges into the current `main`.** Conflicts may be resolved by hand only when
   you understand both sides; then the resolution is reviewed like the rest.
3. **On the merged tree:** `cargo fmt --all --check`, `cargo clippy --all-targets`,
   `cargo test` pass — run once, in a target directory no agent builds into.

`skills/expl-adopt/scripts/check-hypothesis.sh N` checks 1–3 in a throwaway worktree
(target dir `.expl/check-target`) and exits non-zero with the reason.

## Review (the judgement; do not skip)

Review two things: the node, and the whole it joins.

**The node.** Read the branch's own change, not only its summary:
`git diff $(git merge-base main expl/node-N) expl/node-N`. Its path comes with it —
`git log --oneline main..expl/node-N` lists every predecessor step that would be
adopted too; each is reviewed as well.

- Does it do what its objective and summary claim? Trace the real code path, not
  only the tests.
- Correctness: concurrency between processes and replicas, SQLite transactions,
  lease/attempt accounting, migrations on an existing database, error paths that
  silently drop or permanently fail work, anything that kills or deletes.
- Tests: would they fail with the feature broken? Tests that pass by construction,
  counterexample "policies" that exist only in the test suite, or slow brittle
  end-to-end scripts are maintenance, not progress.
- No stubs, deleted tests, or `allow(...)` sprinkled to silence lints.

**The whole.** Read what the merge actually changes in `main`:
merge into a scratch worktree and `git diff main HEAD` there.

- Auto-merged hunks in files `main` also changed since the branch's base are where
  regressions hide: a guard another adopted step added can be silently undone
  (adopting #65 removed #22's missing-commit guard this way). Look at every file
  both sides touched.
- Does it contradict an assumption another adopted change relies on, or AGENTS.md?
- Does `main` need it — or does it duplicate, half-implement, or complicate what is
  there? Several competing hypotheses: take the one that best serves the goals with
  the least added surface; the others stay in the graph as alternatives.

For a non-trivial diff, get a second read that is not anchored on yours: give a
reviewer agent the commits and AGENTS.md (not your conclusion) and ask for
correctness bugs, interactions with `main`, and test quality. Verify each finding
in the code before acting on it.

**Decide.** Merge only when the review finds nothing that makes `main` worse. A
correct but incomplete piece may still go in when it is a needed part of a goal and
says honestly what it leaves out. When the review finds a problem, do not merge;
note it for the owner (and, if a smaller or fixed version would be worth having,
add it to the graph yourself with `expl node add`).

## Procedure

```sh
expl goal ls; expl hypothesis ls                    # what the goals need; what is on offer
expl node inspect N | less                          # objective, outcome, evaluations
git log --oneline main..expl/node-N                 # the whole path that would come in
skills/expl-adopt/scripts/check-hypothesis.sh N      # the gate
# review: the node's diff, then the merged diff against main (above)
skills/expl-adopt/scripts/adopt.sh N                # gate again + merge --no-ff + build & install
expl origin ls                                      # confirm: new origin, ADOPTED shows #N
```

Write in the merge commit which goal the change serves and anything the review
accepted knowingly. `adopt.sh` refuses unless you are on a clean `main`; after a
hand-resolved merge, commit in the scratch worktree and fast-forward `main` to it.

`adopt.sh` installs the adopted expl (`skills/expl-operate/scripts/install.sh`);
then run `skills/expl-operate/scripts/rolling-restart.sh` to move the running
processes over as each goes idle. When the adoption adds a database migration,
build the UI as well (`install.sh --ui`) and restart every process and the UI
together, because an old binary refuses a newer schema.

## After merging

- `expl origin ls` should show the new origin with `#N` adopted within a few seconds.
  If it does not, see `expl-troubleshoot` ("trunk not followed").
- Tell the owner what was adopted, which goal it serves, and what the review
  accepted knowingly — one line per adoption.
- Never push, force-push, rebase or rewrite `main`; the lineage depends on its history.
