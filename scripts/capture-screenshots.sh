#!/usr/bin/env bash
# Regenerate docs/screenshot-*.png from the real rscreen client/server.
#
# Needs: cargo, python3 + Pillow, DejaVu Sans Mono.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
chmod +x docs/demo.sh
cargo build --quiet
cargo build --quiet --example snapshot

runtime="$(mktemp -d)"
trap 'rm -rf "$runtime"' EXIT
mkdir -p "$runtime"
cat > "$runtime/config.toml" <<EOF
[general]
shell = "$root/docs/demo.sh"
mouse = true
status_rows = 1
EOF
export RSCREEN_RUNTIME_DIR="$runtime"
export RSCREEN_CONFIG="$runtime/config.toml"
rm -f /tmp/rscreen-demo.count

./target/debug/rscreen new demo --detached >/dev/null
sleep 0.4
snap=./target/debug/examples/snapshot
socket="$runtime/demo.sock"

$snap --socket "$socket" --cols 100 --rows 30 --wait 600 \
  --send 017c --send 0141 --send 15 --send 776f726b73706163650d \
  --settle 2400 --out /tmp/rscreen-main.json
$snap --socket "$socket" --cols 100 --rows 30 --wait 300 --send 013a \
  --out /tmp/rscreen-palette.json
$snap --socket "$socket" --cols 100 --rows 30 --wait 300 --send 013f \
  --out /tmp/rscreen-help.json
$snap --socket "$socket" --cols 100 --rows 30 --wait 300 \
  --send 015b --send 2f --send 746573740d --out /tmp/rscreen-search.json

for name in main palette help search; do
  python3 scripts/render_screenshot.py "/tmp/rscreen-$name.json" "docs/screenshot-$name.png"
done
./target/debug/rscreen kill demo >/dev/null 2>&1 || true
