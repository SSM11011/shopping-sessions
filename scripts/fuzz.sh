#!/usr/bin/env bash
# Differential test: generate seeds, run the Python reference and the Rust binary, compare.
# Usage: scripts/fuzz.sh FIRST_SEED LAST_SEED EVENTS [BIN]
set -u
cd "$(dirname "$0")/.."
FIRST=${1:-1}; LAST=${2:-20}; EVENTS=${3:-20000}; BIN=${4:-target/release/sessions}
CLICKS=$((EVENTS / 4))
WORK=${FUZZ_DIR:-/tmp/claude-1000/fuzz}
mkdir -p "$WORK"
export PATH="$PWD/.venv/bin:$PATH"
one() {
  s=$1
  d="$WORK/s$s-$EVENTS"
  rm -rf "$d"; mkdir -p "$d"
  generate --seed "$s" --events "$EVENTS" --clicks "$CLICKS" --output "$d/data" --threads 2 >/dev/null 2>&1
  python3 tools/reference.py "$d/data" "$d/ref" || { echo "seed $s: REFERENCE CRASH"; return; }
  mkdir -p "$d/out"
  DATA_DIR="$d/data" OUTPUT_DIR="$d/out" NCPU=${NCPU:-4} "$BIN" 2>/dev/null || { echo "seed $s: BIN CRASH"; return; }
  if python3 scripts/compare.py "$d/out" "$d/ref" > "$d/cmp.txt" 2>&1; then
    echo "seed $s: ok"; rm -rf "$d"
  else
    echo "seed $s: DIFF ($d/cmp.txt)"
  fi
}
export -f one; export WORK EVENTS CLICKS BIN
seq "$FIRST" "$LAST" | xargs -P "${JOBS:-8}" -I{} bash -c 'one {}'
