#!/usr/bin/env bash
# bench.sh DATA [runs]: prints wall / user / sys per run plus stage timings
D=${1:-data/p20}; N=${2:-3}
for i in $(seq $N); do
  rm -rf /tmp/claude-1000/ob; mkdir -p /tmp/claude-1000/ob
  /usr/bin/time -f "wall %e user %U sys %S rss %MkB minflt %R ctxsw %c" env DATA_DIR=$D OUTPUT_DIR=/tmp/claude-1000/ob NCPU=8 taskset -c 0-7 target/release/sessions 2>&1 | tr '\n' ' '; echo
done
