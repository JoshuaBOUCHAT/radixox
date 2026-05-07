#!/bin/bash
# Profile RadixOx avec memtier_benchmark — deux phases, deux profils samply.
#
#   Phase 1 LOAD : pure SET séquentiel  → profile_mt_load_<ts>.json
#   Phase 2 RUN  : 1:1 GET:SET Gaussian → profile_mt_run_<ts>.json
#
# Usage: ./profile_memtier.sh
#   KEY_MAX=200000 TEST_TIME=30 ./profile_memtier.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PORT=6379
KEY_MAX="${KEY_MAX:-500000}"
LOAD_TIME="${LOAD_TIME:-30}"
TEST_TIME="${TEST_TIME:-60}"
MT_THREADS="${MT_THREADS:-4}"
MT_CLIENTS="${MT_CLIENTS:-20}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
PROFILE_LOAD="${PROFILE_LOAD:-/tmp/radixox_profile_mt_load_${TIMESTAMP}.json}"
PROFILE_RUN="${PROFILE_RUN:-/tmp/radixox_profile_mt_run_${TIMESTAMP}.json}"

RADIXOX_RELEASE="$SCRIPT_DIR/target/release/radixox"
RADIXOX_PROFILING="$SCRIPT_DIR/target/profiling/radixox"

# ── helpers ──────────────────────────────────────────────────────────────────

wait_for_server() {
  echo -n "  Waiting for server on :$PORT..."
  for i in $(seq 1 60); do
    if redis-cli -p "$PORT" PING 2>/dev/null | grep -q PONG; then
      echo " ready."; return 0
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
      ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT" || return 0
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

start_samply() {
  local profile_out=$1
  kill_port || true
  samply record -o "$profile_out" -- taskset -c 0-1 "$RADIXOX_PROFILING" > /tmp/radixox_samply.log 2>&1 &
  SAMPLY_PID=$!
  sleep 1
  RADIXOX_PID=$(pgrep -P "$SAMPLY_PID" radixox 2>/dev/null || echo "$SAMPLY_PID")
  wait_for_server
}

stop_samply() {
  echo "  Stopping server..."
  kill "$RADIXOX_PID" 2>/dev/null || kill "$SAMPLY_PID" 2>/dev/null || true
  wait "$SAMPLY_PID" 2>/dev/null || true
}

mt_load_silent() {
  taskset -c 2-7 memtier_benchmark \
    --server=127.0.0.1 --port="$PORT" \
    --protocol=redis \
    --threads="$MT_THREADS" --clients="$MT_CLIENTS" \
    --ratio=1:0 \
    --key-pattern=S:S \
    --key-prefix="mt:" \
    --key-minimum=1 --key-maximum="$KEY_MAX" \
    --data-size=100 \
    --test-time="$LOAD_TIME" \
    --hide-histogram \
    > /dev/null 2>&1
}

cleanup() { kill_port 2>/dev/null || true; }
trap cleanup EXIT

# ── build ─────────────────────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════"
echo "  RadixOx profiling — memtier LOAD + RUN"
echo "  Keys: $KEY_MAX | Load: ${LOAD_TIME}s | Run: ${TEST_TIME}s"
echo "  Connections: ${MT_THREADS}t × ${MT_CLIENTS}c"
echo "  Load profile : $PROFILE_LOAD"
echo "  Run  profile : $PROFILE_RUN"
echo "═══════════════════════════════════════════════════"
echo ""
echo "[1/6] Building RadixOx (release + profiling)..."
RUSTFLAGS="-C target-cpu=native" cargo build --release \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
RUSTFLAGS="-C target-cpu=native" cargo build --profile profiling \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
echo "  Builds done."
echo ""

# ── phase 1 : LOAD profilé ────────────────────────────────────────────────────

echo "[2/6] Starting RadixOx (profiling) under samply for LOAD phase..."
start_samply "$PROFILE_LOAD"

echo "[3/6] Running memtier LOAD (${LOAD_TIME}s, pure SET)..."
taskset -c 2-7 memtier_benchmark \
  --server=127.0.0.1 --port="$PORT" \
  --protocol=redis \
  --threads="$MT_THREADS" --clients="$MT_CLIENTS" \
  --ratio=1:0 \
  --key-pattern=S:S \
  --key-prefix="mt:" \
  --key-minimum=1 --key-maximum="$KEY_MAX" \
  --data-size=100 \
  --test-time="$LOAD_TIME" \
  --print-percentiles=50,99,99.9,99.99 \
  --hide-histogram \
  | tee /tmp/radixox_profile_mt_load.txt

stop_samply
echo "  Load profile saved: $PROFILE_LOAD"
echo ""

# ── phase 2 : RUN profilé ─────────────────────────────────────────────────────
# RadixOx est in-memory : on peuple d'abord vite avec le binaire release,
# puis on relance avec samply+profiling pour profiler uniquement la phase run.

echo "[4/6] Starting RadixOx (release) to repopulate data..."
kill_port || true
taskset -c 0-1 "$RADIXOX_RELEASE" &
RELEASE_PID=$!
wait_for_server

echo "  Loading $KEY_MAX keys (no profiling)..."
mt_load_silent
echo "  Done. Stopping release server..."
kill "$RELEASE_PID" 2>/dev/null || true
wait "$RELEASE_PID" 2>/dev/null || true
kill_port || true
echo ""

echo "[5/6] Starting RadixOx (profiling) under samply for RUN phase..."
start_samply "$PROFILE_RUN"

echo "  Reloading $KEY_MAX keys into profiling instance..."
mt_load_silent
echo "  Data ready. Starting profiled run..."

echo "[6/6] Running memtier RUN (${TEST_TIME}s, 1:1 GET:SET, Gaussian)..."
taskset -c 2-7 memtier_benchmark \
  --server=127.0.0.1 --port="$PORT" \
  --protocol=redis \
  --threads="$MT_THREADS" --clients="$MT_CLIENTS" \
  --ratio=1:1 \
  --key-pattern=G:G \
  --key-prefix="mt:" \
  --key-minimum=1 --key-maximum="$KEY_MAX" \
  --data-size=100 \
  --test-time="$TEST_TIME" \
  --print-percentiles=50,99,99.9,99.99 \
  --hide-histogram \
  | tee /tmp/radixox_profile_mt_run.txt

stop_samply
echo "  Run profile saved: $PROFILE_RUN"
echo ""

# ── résumé ────────────────────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════"
echo "  Profils générés :"
echo "    samply load $PROFILE_LOAD"
echo "    samply load $PROFILE_RUN"
echo "  Logs memtier : /tmp/radixox_profile_mt_{load,run}.txt"
echo "═══════════════════════════════════════════════════"
