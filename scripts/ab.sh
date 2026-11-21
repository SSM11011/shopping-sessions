#!/usr/bin/env bash
# ab.sh BIN_A BIN_B [runs] [data]: interleaved runs, prints min/median wall per binary
A=$1; B=$2; N=${3:-7}; D=${4:-data/p20}
for i in $(seq $N); do
  for X in A B; do
    BIN=${!X}; rm -rf /tmp/claude-1000/oab; mkdir -p /tmp/claude-1000/oab
    t=$( { /usr/bin/time -f '%e' env SESSIONS_QUIET=1 DATA_DIR=$D OUTPUT_DIR=/tmp/claude-1000/oab NCPU=8 taskset -c 0-7 $BIN; } 2>&1 | tail -1)
    echo "$X $t"
  done
done | sort -k1,1 -k2,2n | awk '{a[$1]=a[$1]" "$2} END{for(k in a){n=split(a[k],v," "); print k, "min", v[1], "med", v[int((n+1)/2)]}}'
