#!/bin/bash
# Profile RadixOx YCSB load phase (INSERT/HSET heavy) with samply.
# Usage: ./profile_load.sh
# Output: /tmp/radixox_profile_load_<timestamp>.json  (opened in browser by samply)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"
PORT=6379
RECORDS="${RECORDS:-1000000}"
THREADS="${THREADS:-400}"
FIELDLENGTH="${FIELDLENGTH:-100}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
PROFILE_OUT="${PROFILE_OUT:-/tmp/radixox_profile_load_${TIMESTAMP}.json}"

RADIXOX_BIN="$SCRIPT_DIR/target/profiling/radixox"

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
echo "  RadixOx profiling — LOAD phase"
echo "  Records: $RECORDS | Threads: $THREADS | Field: ${FIELDLENGTH}B"
echo "  Profile output: $PROFILE_OUT"
echo "═══════════════════════════════════════════════════"
echo ""
echo "[1/3] Building RadixOx (profiling)..."
RUSTFLAGS="-C target-cpu=native" cargo build --profile profiling \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
echo "  Build done: $RADIXOX_BIN"
echo ""

# ── start server under samply ────────────────────────────────────────────────

echo "[2/3] Starting RadixOx under samply on :$PORT..."
kill_port || true

samply record -o "$PROFILE_OUT" -- taskset -c 0-1 "$RADIXOX_BIN" > /tmp/radixox_samply_load.log 2>&1 &
SAMPLY_PID=$!

# Give samply a moment to fork the child, then find radixox pid
sleep 1
RADIXOX_PID=$(pgrep -P "$SAMPLY_PID" radixox 2>/dev/null || true)
if [ -z "$RADIXOX_PID" ]; then
  # Fallback: samply may exec directly (no intermediate child on some versions)
  RADIXOX_PID=$SAMPLY_PID
fi

wait_for_server

# ── ycsb load ────────────────────────────────────────────────────────────────

echo "[3/3] Running YCSB load ($RECORDS records)..."
taskset -c 2-5 "$YCSB_DIR/bin/ycsb.sh" load redis -s \
  -P "$YCSB_DIR/$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port="$PORT" \
  -p fieldcount=100 -p fieldnamekey=false \
  -p fieldlength="$FIELDLENGTH" \
  -p recordcount="$RECORDS" \
  -threads "$THREADS" \
  -p percentiles=95,99,99.9,99.99 \
  | tee /tmp/radixox_profile_load_ycsb.txt

echo ""
echo "YCSB done. Stopping server..."
# Kill radixox so samply finalises the profile
kill "$RADIXOX_PID" 2>/dev/null || kill "$SAMPLY_PID" 2>/dev/null || true
wait "$SAMPLY_PID" 2>/dev/null || true

echo ""
echo "Profile saved: $PROFILE_OUT"
echo "Open manually: samply load $PROFILE_OUT"
