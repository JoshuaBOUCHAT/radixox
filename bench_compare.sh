#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"
PORT=6379
RECORDS=1000000
OPS=2000000
THREADS=100
FIELDLENGTH=100

RADIXOX_BIN="$SCRIPT_DIR/target/lto/radixox-resp"
CURRENT_LOG=""

# ============================================
# Helpers
# ============================================

cleanup() {
  local exit_code=$?
  if [ $exit_code -ne 0 ]; then
    echo ""
    echo "ERROR: script exited with code $exit_code"
    if [ -n "$CURRENT_LOG" ] && [ -f "$CURRENT_LOG" ]; then
      echo "--- Last lines of $CURRENT_LOG ---"
      tail -30 "$CURRENT_LOG"
    fi
  fi
  kill_port 2>/dev/null || true
}
trap cleanup EXIT

wait_for_server() {
  echo -n "  Waiting for server on :$PORT..."
  for i in $(seq 1 60); do
    if redis-cli -p "$PORT" PING 2>/dev/null | grep -q PONG; then
      echo " ready."
      return 0
    fi
    sleep 0.5
  done
  echo " TIMEOUT"
  return 1
}

kill_port() {
  local pid
  pid=$(ss -tlnp "sport = :$PORT" 2>/dev/null | grep -oP '(?<=pid=)\d+' | head -1 || true)
  if [ -n "$pid" ]; then
    echo "  Stopping server (pid $pid)..."
    kill "$pid" 2>/dev/null || true
    # Wait until port is free
    for i in $(seq 1 20); do
      if ! ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT"; then
        return 0
      fi
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

run_ycsb() {
  local phase=$1 # load or run
  local outfile=$2
  local extra_args="${3:-}"

  if [ "$phase" = "load" ]; then
    taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" load redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host=127.0.0.1 -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p recordcount="$RECORDS" \
      -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 \
      $extra_args \
      >"$outfile" 2>&1
  else
    taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" run redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host=127.0.0.1 -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p operationcount="$OPS" \
      -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 \
      $extra_args \
      >"$outfile" 2>&1
  fi
}

parse_stat() {
  local file=$1
  local metric=$2
  grep -F "$metric" "$file" 2>/dev/null | awk '{print $3}' | head -1 || true
}

# Parse p99.9 / p99.99 from the inline status line:
# "... [READ: Count=N, ..., 99.9=740, 99.99=4591] ..."
parse_inline_pct() {
  local file=$1
  local op=$2  # READ, UPDATE, INSERT
  local pct=$3 # 99.9 or 99.99
  grep -oP "\[$op:[^\]]*\]" "$file" 2>/dev/null | tail -1 |
    grep -oP "${pct}=\K[0-9]+" || true
}

# ============================================
# Build RadixOx
# ============================================
echo "═══════════════════════════════════════════════════"
echo "  YCSB Benchmark: RadixOx vs Redis"
echo "  Workload A: 50% read / 50% update"
echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS"
echo "  Field length: $FIELDLENGTH bytes"
echo "═══════════════════════════════════════════════════"
echo ""
echo "[0/4] Building RadixOx (release)..."
cargo build -p radixox-server --profile lto --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 |
  tail -3
echo "  Build done: $RADIXOX_BIN"
echo ""

# ============================================
# RadixOx Benchmark
# ============================================
echo "[1/4] Starting RadixOx on :$PORT..."
kill_port || true
taskset -c 0-1 "$RADIXOX_BIN" &
RADIXOX_PID=$!
wait_for_server

echo "[2/4] Benchmarking RadixOx..."
echo "  Loading $RECORDS records..."
CURRENT_LOG=/tmp/radixox_load.txt
run_ycsb load /tmp/radixox_load.txt
RADIXOX_LOAD_OPS=$(parse_stat /tmp/radixox_load.txt "Throughput(ops/sec)")
RADIXOX_LOAD_P99=$(parse_stat /tmp/radixox_load.txt "[INSERT], 99thPercentileLatency")
echo "  Load: $RADIXOX_LOAD_OPS ops/sec | P99: $RADIXOX_LOAD_P99 µs"

echo "  Running $OPS operations..."
CURRENT_LOG=/tmp/radixox_run.txt
run_ycsb run /tmp/radixox_run.txt
RADIXOX_RUN_OPS=$(parse_stat /tmp/radixox_run.txt "Throughput(ops/sec)")
RADIXOX_READ_AVG=$(parse_stat /tmp/radixox_run.txt "[READ], AverageLatency")
RADIXOX_READ_P95=$(parse_stat /tmp/radixox_run.txt "[READ], 95thPercentileLatency")
RADIXOX_READ_P99=$(parse_stat /tmp/radixox_run.txt "[READ], 99thPercentileLatency")
RADIXOX_READ_P999=$(parse_inline_pct /tmp/radixox_run.txt READ 99.9)
RADIXOX_READ_P9999=$(parse_inline_pct /tmp/radixox_run.txt READ 99.99)
RADIXOX_UPDATE_P99=$(parse_stat /tmp/radixox_run.txt "[UPDATE], 99thPercentileLatency")
echo "  Run: $RADIXOX_RUN_OPS ops/sec"
echo ""

kill_port

# ============================================
# Redis Benchmark
# ============================================
echo "[3/4] Starting Redis on :$PORT..."
taskset -c 0-1 redis-server --port "$PORT" --daemonize no --save "" --appendonly no --dir /tmp &
REDIS_PID=$!
wait_for_server

echo "[4/4] Benchmarking Redis..."
echo "  Loading $RECORDS records..."
CURRENT_LOG=/tmp/redis_load.txt
run_ycsb load /tmp/redis_load.txt
REDIS_LOAD_OPS=$(parse_stat /tmp/redis_load.txt "Throughput(ops/sec)")
REDIS_LOAD_P99=$(parse_stat /tmp/redis_load.txt "[INSERT], 99thPercentileLatency")
echo "  Load: $REDIS_LOAD_OPS ops/sec | P99: $REDIS_LOAD_P99 µs"

echo "  Running $OPS operations..."
CURRENT_LOG=/tmp/redis_run.txt
run_ycsb run /tmp/redis_run.txt
REDIS_RUN_OPS=$(parse_stat /tmp/redis_run.txt "Throughput(ops/sec)")
REDIS_READ_AVG=$(parse_stat /tmp/redis_run.txt "[READ], AverageLatency")
REDIS_READ_P95=$(parse_stat /tmp/redis_run.txt "[READ], 95thPercentileLatency")
REDIS_READ_P99=$(parse_stat /tmp/redis_run.txt "[READ], 99thPercentileLatency")
REDIS_READ_P999=$(parse_inline_pct /tmp/redis_run.txt READ 99.9)
REDIS_READ_P9999=$(parse_inline_pct /tmp/redis_run.txt READ 99.99)
REDIS_UPDATE_P99=$(parse_stat /tmp/redis_run.txt "[UPDATE], 99thPercentileLatency")
echo "  Run: $REDIS_RUN_OPS ops/sec"
echo ""

kill_port

# ============================================
# Comparison
# ============================================
winner() {
  local a=$1 b=$2 higher=$3 # higher=1 means bigger is better
  if [ -z "$a" ] || [ -z "$b" ]; then
    echo "N/A"
    return
  fi
  if [ "$higher" = "1" ]; then
    echo "$a $b" | awk '{print ($1 > $2) ? "RadixOx" : "Redis"}'
  else
    echo "$a $b" | awk '{print ($1 < $2) ? "RadixOx" : "Redis"}'
  fi
}

echo "═══════════════════════════════════════════════════"
echo "  RESULTS COMPARISON"
echo "═══════════════════════════════════════════════════"
printf "%-28s %12s %12s %10s\n" "Metric" "RadixOx" "Redis" "Winner"
echo "-----------------------------------------------------------"
printf "%-28s %12s %12s %10s\n" "Load throughput (ops/sec)" \
  "$RADIXOX_LOAD_OPS" "$REDIS_LOAD_OPS" \
  "$(winner "$RADIXOX_LOAD_OPS" "$REDIS_LOAD_OPS" 1)"
printf "%-28s %12s %12s %10s\n" "Load P99 (µs)" \
  "$RADIXOX_LOAD_P99" "$REDIS_LOAD_P99" \
  "$(winner "$RADIXOX_LOAD_P99" "$REDIS_LOAD_P99" 0)"
echo ""
printf "%-28s %12s %12s %10s\n" "Run throughput (ops/sec)" \
  "$RADIXOX_RUN_OPS" "$REDIS_RUN_OPS" \
  "$(winner "$RADIXOX_RUN_OPS" "$REDIS_RUN_OPS" 1)"
printf "%-28s %12s %12s %10s\n" "READ avg (µs)" \
  "$RADIXOX_READ_AVG" "$REDIS_READ_AVG" \
  "$(winner "$RADIXOX_READ_AVG" "$REDIS_READ_AVG" 0)"
printf "%-28s %12s %12s %10s\n" "READ P95 (µs)" \
  "$RADIXOX_READ_P95" "$REDIS_READ_P95" \
  "$(winner "$RADIXOX_READ_P95" "$REDIS_READ_P95" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99 (µs)" \
  "$RADIXOX_READ_P99" "$REDIS_READ_P99" \
  "$(winner "$RADIXOX_READ_P99" "$REDIS_READ_P99" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99.9 (µs)" \
  "$RADIXOX_READ_P999" "$REDIS_READ_P999" \
  "$(winner "$RADIXOX_READ_P999" "$REDIS_READ_P999" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99.99 (µs)" \
  "$RADIXOX_READ_P9999" "$REDIS_READ_P9999" \
  "$(winner "$RADIXOX_READ_P9999" "$REDIS_READ_P9999" 0)"
printf "%-28s %12s %12s %10s\n" "UPDATE P99 (µs)" \
  "$RADIXOX_UPDATE_P99" "$REDIS_UPDATE_P99" \
  "$(winner "$RADIXOX_UPDATE_P99" "$REDIS_UPDATE_P99" 0)"
echo ""
echo "Full logs: /tmp/radixox_{load,run}.txt  /tmp/redis_{load,run}.txt"
