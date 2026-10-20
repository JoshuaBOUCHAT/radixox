#!/bin/bash
# Test de stress : load 1M clés puis 80% GET / 20% SET, avec ou sans BGSAVE.
set -uo pipefail
# Pas de save automatique pendant un bench (Valkey y tourne avec --save "").
export RADIXOX_SAVE="${RADIXOX_SAVE-}"
cd /home/joshua/rust/radixox
S=${OUT_DIR:-/tmp/stress_save}; mkdir -p "$S"
BIN=target/release/radixox; PORT=6390
# SERVER=valkey : même test sur Valkey (1 thread, RDB écrit dans $S, fork pour BGSAVE)
if [ "${SERVER:-radixox}" = valkey ]; then
  SRV=(valkey-server --port $PORT --save "" --appendonly no --dir $S --dbfilename dump.rdb)
else SRV=($BIN); fi
MT="taskset -c 2-7 memtier_benchmark -s 127.0.0.1 -p $PORT --protocol=redis -t 4 -c 20 --key-prefix=k: --key-minimum=1 --key-maximum=1000000 --data-size=100 --pipeline=${PIPELINE:-1} --hide-histogram --print-percentiles=50,99,99.9,99.99"
for mode in ${MODES:-nosave save}; do
  fuser -k $PORT/tcp >/dev/null 2>&1; sleep 0.5
  systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
    env RADIXOX_PORT=$PORT RADIXOX_DIR=$S taskset -c 0-1 "${SRV[@]}" > $S/srv_$mode.log 2>&1 &
  for _ in $(seq 40); do redis-cli -p $PORT PING 2>/dev/null | grep -q PONG && break; sleep 0.25; done
  timeout 30 $MT --ratio=1:0 --key-pattern=P:P --requests=allkeys > $S/load_$mode.txt 2>&1
  if [ $mode = save ]; then (sleep 1; redis-cli -p $PORT BGSAVE > $S/bgsave_reply.txt) & BG=$!; fi
  timeout 30 $MT --ratio=${RATIO:-1:4} --key-pattern=R:R --test-time=${RUN_TIME:-15} > $S/run_$mode.txt 2>&1
  [ $mode = save ] && wait $BG
  echo "== $mode"
  grep -E "^Totals" $S/load_$mode.txt | awk '{printf "load  ops/s %s\n", $2}'
  grep -E "^Type|^Gets|^Sets|^Totals" $S/run_$mode.txt
  grep -E "bgsave|Background saving|RDB:" $S/srv_$mode.log || true
  [ "${SERVER:-radixox}" = valkey ] && redis-cli -p $PORT INFO all | grep -E "rdb_last_bgsave_time_sec|latest_fork_usec|rdb_last_cow_size|rdb_bgsave_in_progress" 
  fuser -k $PORT/tcp >/dev/null 2>&1
done
cat $S/bgsave_reply.txt 2>/dev/null
