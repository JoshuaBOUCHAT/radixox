#!/bin/bash
# Profil IBS (op) du serveur PID pendant DUR s : adresse, source de la donnée
# (L1 / L2 / L3 / DRAM) et latence de chaque micro-op échantillonnée. À lancer
# via scripts/uuid_bench.sh (PROF=scripts/uuid_prof_l2.sh), qui fournit PID,
# OUT et DUR ; analyse : scripts/ibs_mem.py "$OUT".
#
# Par processus (-p) : marche avec perf_event_paranoid = 1, sans root.
set -euo pipefail
PERIOD="${PERIOD:-200003}"
info() { redis-cli -p "${PORT:-6441}" info | tr -d '\r' | awk -F: '/^total_commands_processed:/ {print $2}'; }
cp "/proc/$PID/maps" "$OUT/maps"
c0="$(info)"
perf record -q -e "ibs_op/cnt_ctl=1/" -c "$PERIOD" -d --sample-mem-info --sample-cpu -W --data-mmap \
  -o "$OUT/perf.data" -p "$PID" -- sleep "$DUR" 2>"$OUT/perf.log"
c1="$(info)"
echo "$((c1 - c0)) $PERIOD" >"$OUT/cmds"
perf script -i "$OUT/perf.data" -F comm,ip,sym,addr,data_src,weight 2>/dev/null | gzip -1 >"$OUT/samples.gz"
