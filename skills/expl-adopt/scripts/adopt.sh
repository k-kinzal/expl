#!/usr/bin/env bash
# Merge a checked candidate into the trunk, then put the adopted expl to work.
#
# Usage: skills/expl-adopt/scripts/adopt.sh [--restart] NODE_ID [TRUNK]
#
# Runs check-candidate.sh first and refuses on any failure. The merge is a
# --no-ff merge so the lineage keeps the step's commits as they were made.
# Because the repository is expl itself, the adopted code is built and
# installed over the `expl` on PATH, so processes started from now on run it. With
# --restart every process is restarted at once; this interrupts in-flight
# agent runs (their events are redelivered), so use it when the adoption
# changes the database schema or fixes something the running processes need.
set -euo pipefail
restart=0
if [ "${1:-}" = "--restart" ]; then restart=1; shift; fi
node="${1:?usage: adopt.sh [--restart] NODE_ID [TRUNK]}"
node="${node#\#}"
trunk="${2:-main}"
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git rev-parse --show-toplevel)"
cd "$repo"

[ "$(git branch --show-current)" = "$trunk" ] || { echo "checkout ${trunk} first"; exit 1; }
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "the trunk checkout has uncommitted changes"; exit 1; }

"$here/check-candidate.sh" "$node" "$trunk"

# The branch recorded on the node: a retried step's is expl/node-<id>-<n>.
title="$(expl node inspect "$node" | python3 -c 'import json,sys; print(json.load(sys.stdin)["node"]["title"])')"
branch="$(expl node inspect "$node" | python3 -c 'import json,sys; print(json.load(sys.stdin)["node"]["branch"])')"
git merge --no-ff "$branch" -m "Adopt #${node}: ${title}"

"$here/../../expl-operate/scripts/install.sh"
if git diff --name-only HEAD^1 HEAD | grep -qE 'expl_core/src/(store|graph/graph|event/inbox|process/)'; then
  echo "note: the adoption touches the store, graph, inbox or process code; restart processes soon"
fi
if [ "$restart" = 1 ]; then
  expl down -t 30 || true
  expl up
  echo "adopted #${node}; processes restarted on the new expl"
else
  echo "adopted #${node}; new expl installed — run skills/expl-operate/scripts/rolling-restart.sh"
fi
