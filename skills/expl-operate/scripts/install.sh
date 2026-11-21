#!/usr/bin/env bash
# Build expl (and optionally expl-ui) from the current checkout and install
# them over the binaries on PATH.
#
# Usage: skills/expl-operate/scripts/install.sh [--ui]
#
# Builds in the repository's own target/ (incremental; `cargo install`
# rebuilds every dependency, gpui included, for 20+ minutes). Installs by
# copying to a temporary name and renaming over the old binary: on macOS,
# overwriting a binary in place (plain `cp`) invalidates its code signature
# and every later launch is killed on the spot, silently.
set -euo pipefail
repo="$(git rev-parse --show-toplevel)"
cd "$repo"
bin="$(dirname "$(command -v expl)")"

place() {
  cp "target/release/$1" "$bin/$1.new"
  mv -f "$bin/$1.new" "$bin/$1"
  "$bin/$1" --version >/dev/null || { echo "installed $1 does not run" >&2; exit 2; }
  echo "installed $bin/$1"
}

cargo build --release --locked --quiet -p expl_cli
place expl
if [ "${1:-}" = "--ui" ]; then
  cargo build --release --locked --quiet -p expl_ui
  place expl-ui
fi
