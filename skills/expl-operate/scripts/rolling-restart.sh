#!/usr/bin/env bash
# Restart every process that runs an older expl binary than the installed
# one, each at a moment it is idle ("waiting for events" or "paused"), or
# right after it moved on to a new event (an executor leases the next step
# at once, so it is never seen idle; at most INTERVAL seconds of the new
# run are lost). In-flight agent runs are never cut short otherwise. Exits
# when all processes run the installed binary.
#
# Usage: skills/expl-operate/scripts/rolling-restart.sh [REPO] [INTERVAL_SECONDS]
set -uo pipefail
repo="${1:-$(git rev-parse --show-toplevel)}"
interval="${2:-15}"
cd "$repo" || exit 2
binary="$(command -v expl)"
installed=$(stat -f %m "$binary")
state="$(mktemp -d "${TMPDIR:-/tmp}/expl-restart-XXXX")"
trap 'rm -rf "$state"' EXIT

expl --version >/dev/null || { echo "the installed expl does not run" >&2; exit 2; }

while true; do
  stale=0
  listing="$(expl ps --format json)" || { echo "expl ps failed" >&2; exit 2; }
  while IFS=$'\t' read -r name started activity; do
    [ -n "$name" ] || continue
    [ "$started" -ge "$installed" ] && continue
    stale=$((stale + 1))
    previous="$(cat "$state/$name" 2>/dev/null || true)"
    echo "$activity" > "$state/$name"
    moved_on=0
    if [ -n "$previous" ] && [ "$previous" != "$activity" ]; then moved_on=1; fi
    case "$activity" in
      "waiting for events"*|paused*|"") restart=1 ;;
      *) restart=$moved_on ;;
    esac
    if [ "$restart" = 1 ]; then
      echo "$(date -u +%FT%TZ) restarting $name (${activity:-idle})"
      expl stop "$name" -t 30 >/dev/null && expl up >/dev/null 2>&1
    fi
  done < <(printf '%s\n' "$listing" | python3 -c '
import json, sys
from datetime import datetime
for line in sys.stdin:
    p = json.loads(line)
    started = int(datetime.fromisoformat(p["created_at"].replace("Z", "+00:00")).timestamp())
    print(p["name"], started, p.get("activity") or "", sep="\t")')
  if [ "$stale" -eq 0 ]; then
    echo "$(date -u +%FT%TZ) every process runs the installed expl"
    exit 0
  fi
  sleep "$interval"
done
