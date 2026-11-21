#!/usr/bin/env bash
# Decide whether a candidate step may be merged into the trunk.
#
# Usage: skills/expl-adopt/scripts/check-candidate.sh NODE_ID [TRUNK]
#
# Checks, in a throwaway worktree so the main checkout is never touched:
#   1. the branch expl/node-<id> exists;
#   2. it adds, modifies, or deletes no Markdown (*.md) file — they are frozen;
#   3. it merges into the trunk without conflicts;
#   4. on the merged tree, cargo fmt --check, cargo clippy --all-targets and
#      cargo test pass.
# Exit 0 means "safe to adopt"; anything else prints why not.
set -euo pipefail
node="${1:?usage: check-candidate.sh NODE_ID [TRUNK]}"
node="${node#\#}"
trunk="${2:-main}"
repo="$(git rev-parse --show-toplevel)"
branch="expl/node-${node}"
cd "$repo"

git show-ref --verify --quiet "refs/heads/${branch}" || { echo "no branch ${branch}"; exit 1; }

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

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/.expl/cargo-target}"
cd "$work"
cargo fmt --all --check || { echo "REJECT: cargo fmt --check fails"; exit 5; }
cargo clippy --all-targets --quiet 2>&1 | tail -20
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "REJECT: cargo clippy fails"; exit 5; }
cargo test --quiet 2>&1 | grep -E 'test result|FAILED|panicked|error' || true
cargo test --quiet >/dev/null 2>&1 || { echo "REJECT: cargo test fails"; exit 5; }

echo "OK: ${branch} is safe to adopt into ${trunk}"
git -C "$repo" log --oneline "${trunk}..${branch}"
git -C "$repo" diff --stat "$base" "$branch" | tail -1
