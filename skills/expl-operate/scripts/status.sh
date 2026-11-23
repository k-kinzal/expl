#!/usr/bin/env bash
# One-screen status of a running expl: processes, graph, hypotheses that satisfy a goal,
# lineage, failing deliveries and recent agent runs.
#
# Usage: skills/expl-operate/scripts/status.sh [REPO]
set -uo pipefail
repo="${1:-$(git rev-parse --show-toplevel)}"
cd "$repo" || exit 2

section() { printf '\n== %s ==\n' "$1"; }

section "processes"
expl ps
section "consumers"
expl consumer ls
section "goals"
expl goal ls
section "nodes"
expl node ls
section "hypotheses"
expl hypothesis ls
section "lineage"
expl origin ls
section "recent events"
expl event ls --limit 15
section "failed agent runs (newest 5)"
grep -l -E '^=== (exit: exit status: [1-9]|interrupted|.* failed:)' .expl/logs/*.log 2>/dev/null | tail -5
section "process log tails"
for log in .expl/proc/*.log; do
  [ -f "$log" ] || continue
  tail -n 2 "$log"
done
