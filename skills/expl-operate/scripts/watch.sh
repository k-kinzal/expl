#!/usr/bin/env bash
# Stream the process-log lines a controller acts on, from every process —
# including processes started after the watch began. Polls, because
# `tail -F` on macOS does not pick up log files created later.
# Works with the bash 3.2 macOS ships (no associative arrays).
#
# Usage: skills/expl-operate/scripts/watch.sh [REPO] [INTERVAL_SECONDS]
set -uo pipefail
repo="${1:-$(git rev-parse --show-toplevel)}"
interval="${2:-10}"
pattern='completed on|failed on|failed:|giving up|trunk .* moved|cannot follow|exiting|/100|explored (forward|backward)'
state="$(mktemp -d "${TMPDIR:-/tmp}/expl-watch-XXXX")"
trap 'rm -rf "$state"' EXIT
for log in "$repo"/.expl/proc/*.log; do
  [ -f "$log" ] && wc -l < "$log" | tr -d ' ' > "$state/$(basename "$log")"
done
while true; do
  for log in "$repo"/.expl/proc/*.log; do
    [ -f "$log" ] || continue
    mark="$state/$(basename "$log")"
    start=$(cat "$mark" 2>/dev/null || echo 0)
    total=$(wc -l < "$log" | tr -d ' ')
    if [ "$total" -gt "$start" ]; then
      sed -n "$((start + 1)),${total}p" "$log" | grep -E "$pattern"
      echo "$total" > "$mark"
    fi
  done
  sleep "$interval"
done
