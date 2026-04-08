#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"

# ============================================
# Config — override via env or args
# ============================================
HOST="${1:-192.168.1.24}"
PORT="${2:-6379}"
RECORDS="${RECORDS:-500000}"
OPS="${OPS:-1000000}"
THREADS="${THREADS:-100}"
FIELDLENGTH="${FIELDLENGTH:-100}"

# ============================================
# Helpers
# ============================================

wait_for_server() {
  echo -n "  Checking connection to $HOST:$PORT..."
  for i in $(seq 1 10); do
    if redis-cli -h "$HOST" -p "$PORT" PING 2>/dev/null | grep -q PONG; then
      echo " OK"
      return 0
    fi
    sleep 1
  done
  echo " FAILED — server unreachable"
  exit 1
}

run_ycsb() {
  local phase=$1
  local outfile=$2

  if [ "$phase" = "load" ]; then
    "$YCSB_DIR/bin/ycsb.sh" load redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host="$HOST" -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p recordcount="$RECORDS" \
      -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 \
      >"$outfile" 2>&1
  else
    "$YCSB_DIR/bin/ycsb.sh" run redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host="$HOST" -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p operationcount="$OPS" \
      -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 \
      >"$outfile" 2>&1
  fi
}

parse_stat() {
  grep -F "$2" "$1" 2>/dev/null | awk '{print $3}' | head -1 || true
}

parse_inline_pct() {
  local file=$1 op=$2 pct=$3
  grep -oP "\[$op:[^\]]*\]" "$file" 2>/dev/null | tail -1 |
    grep -oP "${pct}=\K[0-9]+" || true
}

# ============================================
# Main
# ============================================
echo "═══════════════════════════════════════════════════"
echo "  YCSB Benchmark — RadixOx remote"
echo "  Target : $HOST:$PORT"
echo "  Workload A : 50% read / 50% update"
echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS"
echo "  Field length: $FIELDLENGTH bytes"
echo "═══════════════════════════════════════════════════"
echo ""

wait_for_server

echo "[1/2] Loading $RECORDS records..."
run_ycsb load /tmp/radixox_remote_load.txt
LOAD_OPS=$(parse_stat /tmp/radixox_remote_load.txt "Throughput(ops/sec)")
LOAD_P99=$(parse_stat /tmp/radixox_remote_load.txt "[INSERT], 99thPercentileLatency")
echo "  Throughput : $LOAD_OPS ops/sec"
echo "  P99        : $LOAD_P99 µs"
echo ""

echo "[2/2] Running $OPS operations..."
run_ycsb run /tmp/radixox_remote_run.txt
RUN_OPS=$(parse_stat /tmp/radixox_remote_run.txt "Throughput(ops/sec)")
READ_AVG=$(parse_stat /tmp/radixox_remote_run.txt "[READ], AverageLatency")
READ_P95=$(parse_stat /tmp/radixox_remote_run.txt "[READ], 95thPercentileLatency")
READ_P99=$(parse_stat /tmp/radixox_remote_run.txt "[READ], 99thPercentileLatency")
READ_P999=$(parse_inline_pct /tmp/radixox_remote_run.txt READ 99.9)
READ_P9999=$(parse_inline_pct /tmp/radixox_remote_run.txt READ 99.99)
UPDATE_P99=$(parse_stat /tmp/radixox_remote_run.txt "[UPDATE], 99thPercentileLatency")
echo ""

echo "═══════════════════════════════════════════════════"
echo "  RESULTS — RadixOx @ $HOST:$PORT"
echo "═══════════════════════════════════════════════════"
printf "%-30s %s\n" "Load throughput (ops/sec)" "$LOAD_OPS"
printf "%-30s %s\n" "Load P99 (µs)" "$LOAD_P99"
echo "---"
printf "%-30s %s\n" "Run throughput (ops/sec)" "$RUN_OPS"
printf "%-30s %s\n" "READ avg (µs)" "$READ_AVG"
printf "%-30s %s\n" "READ P95 (µs)" "$READ_P95"
printf "%-30s %s\n" "READ P99 (µs)" "$READ_P99"
printf "%-30s %s\n" "READ P99.9 (µs)" "$READ_P999"
printf "%-30s %s\n" "READ P99.99 (µs)" "$READ_P9999"
printf "%-30s %s\n" "UPDATE P99 (µs)" "$UPDATE_P99"
echo ""
echo "Full logs: /tmp/radixox_remote_{load,run}.txt"
