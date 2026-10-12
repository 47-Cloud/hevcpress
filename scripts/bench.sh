#!/usr/bin/env bash
# Compare hevcpress with general-purpose compressors on your own files.
#   scripts/bench.sh path/to/IMG_0001.HEIC path/to/IMG_0001.MOV ...
# Uses whichever of xz, zstd and brotli are installed.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin="$here/target/release/hevcpress"
[ -x "$bin" ] || (cd "$here" && cargo build --release >/dev/null)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

orig=0; th=0; tx=0; tz=0; havex=0; havez=0
row() { printf "%-28s %12s %12s %12s %12s\n" "$@"; }
row file original hevcpress xz zstd
for f in "$@"; do
  n=$(wc -c < "$f"); orig=$((orig + n))
  "$bin" compress "$f" "$tmp/out.hvp" 2>/dev/null; h=$(wc -c < "$tmp/out.hvp")
  x=-; z=-
  command -v xz >/dev/null && x=$(xz -9e -c "$f" | wc -c)
  command -v zstd >/dev/null && z=$(zstd -q --ultra -22 -c "$f" | wc -c)
  th=$((th + h))
  if [ "$x" != - ]; then tx=$((tx + x)); havex=1; fi
  if [ "$z" != - ]; then tz=$((tz + z)); havez=1; fi
  row "$(basename "$f")" "$n" "$h" "$x" "$z"
done
pct() { awk -v a="$1" -v b="$orig" 'BEGIN{printf "%.2f%%", 100*(1-a/b)}'; }
sx=-; sz=-
[ "$havex" = 1 ] && sx=$(pct "$tx")
[ "$havez" = 1 ] && sz=$(pct "$tz")
row "saving" "" "$(pct "$th")" "$sx" "$sz"
