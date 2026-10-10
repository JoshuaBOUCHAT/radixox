#!/bin/bash
# Test de stress : load 1M clés puis 80% GET / 20% SET, avec ou sans BGSAVE.
set -uo pipefail
cd /home/joshua/rust/radixox
S=${OUT_DIR:-/tmp/stress_save}; mkdir -p "$S"
BIN=target/release/radixox; PORT=6390
MT="taskset -c 2-7 memtier_benchmark -s 127.0.0.1 -p $PORT --protocol=redis -t 4 -c 20 --key-prefix=k: --key-minimum=1 --key-maximum=1000000 --data-size=100 --hide-histogram --print-percentiles=50,99,99.9,99.99"
for mode in nosave save; do
  fuser -k $PORT/tcp >/dev/null 2>&1; sleep 0.5
  systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
    env RADIXOX_PORT=$PORT taskset -c 0-1 $BIN > $S/srv_$mode.log 2>&1 &
  for _ in $(seq 40); do redis-cli -p $PORT PING 2>/dev/null | grep -q PONG && break; sleep 0.25; done
  timeout 30 $MT --ratio=1:0 --key-pattern=P:P --requests=allkeys > $S/load_$mode.txt 2>&1
  if [ $mode = save ]; then (sleep 1; redis-cli -p $PORT BGSAVE > $S/bgsave_reply.txt) & fi
  timeout 30 $MT --ratio=1:4 --key-pattern=R:R --test-time=15 > $S/run_$mode.txt 2>&1
  wait
  echo "== $mode"
  grep -E "^Totals" $S/load_$mode.txt | awk '{printf "load  ops/s %s\n", $2}'
  grep -E "^Type|^Gets|^Sets|^Totals" $S/run_$mode.txt
  grep bgsave $S/srv_$mode.log || true
  fuser -k $PORT/tcp >/dev/null 2>&1
done
cat $S/bgsave_reply.txt 2>/dev/null
