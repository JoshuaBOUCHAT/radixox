#!/bin/bash
# Profile RadixOx YCSB run phase (50% READ / 50% UPDATE) with samply.
# Strategy: populate data fast with release build, then restart with profiling build + samply.
# Usage: ./profile_run.sh
# Output: /tmp/radixox_profile_run_<timestamp>.json  (opened in browser by samply)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"
PORT=6379
RECORDS="${RECORDS:-1000000}"
OPS="${OPS:-3000000}"
THREADS="${THREADS:-100}"
FIELDLENGTH="${FIELDLENGTH:-100}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
PROFILE_OUT="${PROFILE_OUT:-/tmp/radixox_profile_run_${TIMESTAMP}.json}"

RADIXOX_RELEASE="$SCRIPT_DIR/target/release/radixox"
RADIXOX_PROFILING="$SCRIPT_DIR/target/profiling/radixox"

# ── helpers ──────────────────────────────────────────────────────────────────

wait_for_server() {
  echo -n "  Waiting for server on :$PORT..."
  for i in $(seq 1 60); do
    if redis-cli -p "$PORT" PING 2>/dev/null | grep -q PONG; then
      echo " ready."
      return 0
    fi
    sleep 0.5
  done
  echo " TIMEOUT"; return 1
}

kill_port() {
  local pid
  pid=$(ss -tlnp "sport = :$PORT" 2>/dev/null | grep -oP '(?<=pid=)\d+' | head -1 || true)
  if [ -n "$pid" ]; then
    echo "  Stopping server (pid $pid)..."
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      if ! ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT"; then return 0; fi
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

cleanup() {
  kill_port 2>/dev/null || true
}
trap cleanup EXIT

# ── build ────────────────────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════"
echo "  RadixOx profiling — RUN phase (50R/50U)"
echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS | Field: ${FIELDLENGTH}B"
echo "  Profile output: $PROFILE_OUT"
echo "═══════════════════════════════════════════════════"
echo ""
echo "[1/5] Building RadixOx (release + profiling)..."
RUSTFLAGS="-C target-cpu=native" cargo build --release \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
RUSTFLAGS="-C target-cpu=native" cargo build --profile profiling \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
echo "  Builds done."
echo ""

# ── phase 1: fast load with release build ────────────────────────────────────

echo "[2/5] Starting RadixOx (release) for data population..."
kill_port || true
taskset -c 0-1 "$RADIXOX_RELEASE" &
RELEASE_PID=$!
wait_for_server

echo "[3/5] Loading $RECORDS records (no profiling)..."
taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" load redis -s \
  -P "$YCSB_DIR/$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port="$PORT" \
  -p fieldcount=1 -p fieldnamekey=false \
  -p fieldlength="$FIELDLENGTH" \
  -p recordcount="$RECORDS" \
  -threads "$THREADS" \
  | grep -E "Throughput|INSERT.*99th" || true

echo "  Load done. Stopping release server..."
kill "$RELEASE_PID" 2>/dev/null || true
wait "$RELEASE_PID" 2>/dev/null || true
kill_port || true
echo ""

# ── phase 2: run with profiling build + samply ───────────────────────────────

# NOTE: RadixOx is in-memory — we restart with fresh data then load again before profiling.
# The load above was just to confirm data volume; we reload under the profiling binary.
echo "[4/5] Starting RadixOx (profiling) under samply..."
samply record --rate 10000 -o "$PROFILE_OUT" -- taskset -c 0-1 "$RADIXOX_PROFILING" > /tmp/radixox_samply_run.log 2>&1 &
SAMPLY_PID=$!

sleep 1
RADIXOX_PID=$(pgrep -P "$SAMPLY_PID" radixox 2>/dev/null || echo "$SAMPLY_PID")
wait_for_server

# Reload data silently into the profiling instance
echo "  Reloading data into profiling instance..."
taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" load redis -s \
  -P "$YCSB_DIR/$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port="$PORT" \
  -p fieldcount=1 -p fieldnamekey=false \
  -p fieldlength="$FIELDLENGTH" \
  -p recordcount="$RECORDS" \
  -threads "$THREADS" \
  > /dev/null 2>&1
echo "  Data ready. Starting profiled run..."

echo "[5/5] Running YCSB run ($OPS ops)..."
taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" run redis -s \
  -P "$YCSB_DIR/$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port="$PORT" \
  -p fieldcount=1 -p fieldnamekey=false \
  -p fieldlength="$FIELDLENGTH" \
  -p recordcount="$RECORDS" \
  -p operationcount="$OPS" \
  -threads "$THREADS" \
  -p percentiles=95,99,99.9,99.99 \
  | tee /tmp/radixox_profile_run_ycsb.txt

echo ""
echo "YCSB done. Stopping server..."
kill "$RADIXOX_PID" 2>/dev/null || kill "$SAMPLY_PID" 2>/dev/null || true
wait "$SAMPLY_PID" 2>/dev/null || true

echo ""
echo "Profile saved: $PROFILE_OUT"
echo "Open manually: samply load $PROFILE_OUT"
