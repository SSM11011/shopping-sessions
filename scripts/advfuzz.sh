#!/usr/bin/env bash
# Adversarial differential fuzz: advgen.py datasets, reference vs binary. Usage: advfuzz.sh FIRST LAST
cd "$(dirname "$0")/.."
export PATH="$PWD/.venv/bin:$PATH"
W=/tmp/claude-1000/adv; mkdir -p $W
one() {
  d=$W/s$1; rm -rf $d
  python3 tools/advgen.py $1 $d/data || { echo "seed $1: GEN FAIL"; return; }
  python3 tools/reference.py $d/data $d/ref 2>$d/ref.err || { echo "seed $1: REF CRASH"; return; }
  mkdir -p $d/out
  DATA_DIR=$d/data OUTPUT_DIR=$d/out NCPU=3 SESSIONS_QUIET=1 target/release/sessions 2>$d/bin.err || { echo "seed $1: BIN CRASH"; return; }
  if python3 scripts/compare.py $d/out $d/ref > $d/cmp.txt 2>&1; then echo "seed $1: ok"; rm -rf $d; else echo "seed $1: DIFF $d"; fi
}
export -f one; export W
seq $1 $2 | xargs -P 8 -I{} bash -c 'one {}'
