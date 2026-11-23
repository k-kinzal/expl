#!/usr/bin/env bash
# Decide whether a hypothesis (a step) may be merged into the trunk.
#
# Usage: skills/expl-adopt/scripts/check-hypothesis.sh NODE_ID [TRUNK]
#
# Checks, in a throwaway worktree so the main checkout is never touched:
#   1. the step's recorded branch exists and still points at the commit it
#      recorded (what was evaluated) — the branch is read from the node, since
#      a retried step's branch is expl/node-<id>-<n>, not expl/node-<id>;
#   2. it adds, modifies, or deletes no Markdown (*.md) file — they are frozen;
#   3. it merges into the trunk without conflicts;
#   4. on the merged tree, cargo fmt --check, cargo clippy --all-targets and
#      cargo test pass.
# Exit 0 means "safe to adopt"; anything else prints why not.
set -euo pipefail
node="${1:?usage: check-hypothesis.sh NODE_ID [TRUNK]}"
node="${node#\#}"
trunk="${2:-main}"
repo="$(git rev-parse --show-toplevel)"
cd "$repo"
read -r branch head < <(expl node inspect "$node" | python3 -c '
import json, sys
n = json.load(sys.stdin)["node"]
print(n.get("branch") or "-", n.get("head_commit") or "-")')
[ "$branch" != "-" ] || { echo "#${node} records no branch"; exit 1; }
git show-ref --verify --quiet "refs/heads/${branch}" || { echo "no branch ${branch}"; exit 1; }
tip="$(git rev-parse "refs/heads/${branch}")"
[ "$tip" = "$head" ] || { echo "REJECT: ${branch} is at ${tip}, not the commit #${node} recorded (${head})"; exit 1; }

base="$(git merge-base "$trunk" "$branch")"
md="$(git diff --name-only "$base" "$branch" -- '*.md' '*.MD')"
if [ -n "$md" ]; then
  echo "REJECT: ${branch} touches Markdown files (frozen):"
  echo "$md"
  exit 3
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/expl-check-${node}-XXXX")"
cleanup() { git -C "$repo" worktree remove --force "$work" >/dev/null 2>&1 || true; }
trap cleanup EXIT
git worktree add --quiet --detach "$work" "$trunk"
if ! git -C "$work" merge --quiet --no-ff --no-commit "$branch" >/dev/null 2>&1; then
  echo "REJECT: ${branch} conflicts with ${trunk}:"
  git -C "$work" diff --name-only --diff-filter=U
  exit 4
fi

# Never the agents' target directory: their builds of other worktrees would
# overwrite the binaries this check runs.
export CARGO_TARGET_DIR="$repo/.expl/check-target"
cd "$work"
cargo fmt --all --check || { echo "REJECT: cargo fmt --check fails"; exit 5; }
cargo clippy --all-targets --quiet 2>&1 | tail -20
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "REJECT: cargo clippy fails"; exit 5; }
# One run decides: a second run could pass a flaky test the first failed.
log="$work/.cargo-test.log"
status=0
cargo test --quiet >"$log" 2>&1 || status=$?
grep -E 'test result|FAILED|panicked|error' "$log" || true
[ "$status" -eq 0 ] || { echo "REJECT: cargo test fails"; exit 5; }

echo "OK: ${branch} is safe to adopt into ${trunk}"
git -C "$repo" log --oneline "${trunk}..${branch}"
git -C "$repo" diff --stat "$base" "$branch" | tail -1
