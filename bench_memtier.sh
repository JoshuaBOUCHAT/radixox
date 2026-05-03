#!/bin/bash
# bench_memtier.sh — memtier_benchmark: RadixOx vs Valkey, workloads réalistes
#
# Trois phases :
#   Load     : écriture séquentielle pour peupler le keyspace
#   Caching  : 10 GET / 1 SET, distribution Gaussienne (hot keys), values 100-500B
#   Session  : 1 GET / 1 SET + TTL aléatoire, valeurs 50-200B (JWT/tokens)
#
# Usage : ./bench_memtier.sh
#   VALKEY_IO_THREADS=4 ./bench_memtier.sh   # Valkey multi-thread
#   KEY_MAX=200000 TEST_TIME=30 ./bench_memtier.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PORT=6379

# Keyspace et timing
KEY_MAX="${KEY_MAX:-500000}"
TEST_TIME="${TEST_TIME:-60}"       # secondes par phase benchmarkée
LOAD_TIME="${LOAD_TIME:-20}"       # secondes pour la phase de load

# memtier : 4 threads × 20 clients = 80 connexions
MT_THREADS="${MT_THREADS:-4}"
MT_CLIENTS="${MT_CLIENTS:-20}"

RADIXOX_BIN="$SCRIPT_DIR/target/release/radixox"
VALKEY_IO_THREADS="${VALKEY_IO_THREADS:-1}"

NPROC=$(nproc)
VALKEY_MT_FIRST=$(( NPROC - VALKEY_IO_THREADS - 1 ))
VALKEY_MT_LAST=$(( NPROC - 1 ))

CURRENT_LOG=""

# ============================================================
# Helpers
# ============================================================

cleanup() {
  local ec=$?
  [ $ec -ne 0 ] && echo "" && echo "ERROR: exit code $ec"
  if [ $ec -ne 0 ] && [ -n "$CURRENT_LOG" ] && [ -f "$CURRENT_LOG" ]; then
    echo "--- Last 20 lines of $CURRENT_LOG ---"
    tail -20 "$CURRENT_LOG"
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
  echo " TIMEOUT"; return 1
}

kill_port() {
  local pid
  pid=$(ss -tlnp "sport = :$PORT" 2>/dev/null | grep -oP '(?<=pid=)\d+' | head -1 || true)
  if [ -n "$pid" ]; then
    echo "  Stopping server (pid $pid)..."
    kill "$pid" 2>/dev/null || true
    for i in $(seq 1 20); do
      ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT" || return 0
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

peak_rss_mb() {
  awk '/VmHWM/ {printf "%.0f", $2/1024}' "/proc/$1/status" 2>/dev/null || echo "N/A"
}

# Colonne d'une ligne de sortie memtier (Sets / Gets / Totals)
# Format avec --print-percentiles=50,99,99.9,99.99 :
#   col 1=Type  2=Ops/sec  3=Hits/sec  4=Misses/sec
#       5=Avg.Latency  6=p50  7=p99  8=p99.9  9=p99.99  10=KB/sec
mt_col() {
  local file=$1 row=$2 col=$3
  awk -v r="$row" -v c="$col" '$1 == r {printf "%.2f", $c; exit}' "$file" 2>/dev/null || echo "N/A"
}

# Tronque la partie décimale pour les ops/sec (affichage plus lisible)
round_ops() {
  echo "$1" | awk '{printf "%.0f", $1}' 2>/dev/null || echo "N/A"
}

# ============================================================
# Phases memtier
# ============================================================

# Phase de load : SET séquentiel pour peupler le keyspace
run_load() {
  local outfile=$1
  CURRENT_LOG="$outfile"
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
    > "$outfile" 2>&1
}

# Phase caching : 10 GET pour 1 SET, hot keys (Gaussian), valeurs variées
run_caching() {
  local outfile=$1
  CURRENT_LOG="$outfile"
  taskset -c 2-7 memtier_benchmark \
    --server=127.0.0.1 --port="$PORT" \
    --protocol=redis \
    --threads="$MT_THREADS" --clients="$MT_CLIENTS" \
    --ratio=1:10 \
    --key-pattern=G:G \
    --key-prefix="mt:" \
    --key-minimum=1 --key-maximum="$KEY_MAX" \
    --data-size-range=100-500 \
    --test-time="$TEST_TIME" \
    --print-percentiles=50,99,99.9,99.99 \
    --hide-histogram \
    > "$outfile" 2>&1
}

# Phase session : 1:1, TTL aléatoire 10-300s, valeurs petites (tokens/JWT)
run_session() {
  local outfile=$1
  CURRENT_LOG="$outfile"
  taskset -c 2-7 memtier_benchmark \
    --server=127.0.0.1 --port="$PORT" \
    --protocol=redis \
    --threads="$MT_THREADS" --clients="$MT_CLIENTS" \
    --ratio=1:1 \
    --key-pattern=G:G \
    --key-prefix="sess:" \
    --key-minimum=1 --key-maximum="$KEY_MAX" \
    --data-size-range=50-200 \
    --expiry-range=10-300 \
    --test-time="$TEST_TIME" \
    --print-percentiles=50,99,99.9,99.99 \
    --hide-histogram \
    > "$outfile" 2>&1
}

# Extrait et affiche les stats d'une phase, stocke les valeurs dans des vars globales
extract_phase() {
  local file=$1
  local prefix=$2  # ex: RADIXOX_CACHE ou VALKEY_SESS

  local ops gets_avg gets_p99 gets_p999 gets_p9999 sets_p99
  ops=$(round_ops "$(mt_col "$file" Totals 2)")
  gets_avg=$(mt_col "$file" Gets 5)
  gets_p99=$(mt_col "$file" Gets 7)
  gets_p999=$(mt_col "$file" Gets 8)
  gets_p9999=$(mt_col "$file" Gets 9)
  sets_p99=$(mt_col "$file" Sets 7)

  eval "${prefix}_OPS=$ops"
  eval "${prefix}_GET_AVG=$gets_avg"
  eval "${prefix}_GET_P99=$gets_p99"
  eval "${prefix}_GET_P999=$gets_p999"
  eval "${prefix}_GET_P9999=$gets_p9999"
  eval "${prefix}_SET_P99=$sets_p99"
}

# ============================================================
# Build
# ============================================================
echo "═══════════════════════════════════════════════════════"
echo "  memtier Benchmark: RadixOx vs Valkey"
echo "  Connections: ${MT_THREADS}t × ${MT_CLIENTS}c = $(( MT_THREADS * MT_CLIENTS )) total"
echo "  Keyspace: $KEY_MAX keys | Phase duration: ${TEST_TIME}s"
echo "  Caching: ratio 1:10, Gaussian, 100-500B"
echo "  Session: ratio 1:1,  Gaussian, 50-200B, TTL 10-300s"
[ "$VALKEY_IO_THREADS" -gt 1 ] && echo "  Valkey io-threads: $VALKEY_IO_THREADS"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "[0/6] Building RadixOx (release)..."
RUSTFLAGS="-C target-cpu=native" cargo build --release \
  --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
echo "  Build: $RADIXOX_BIN"
echo ""

# ============================================================
# RadixOx
# ============================================================
echo "[1/6] Starting RadixOx on :$PORT..."
kill_port || true
taskset -c 0-1 "$RADIXOX_BIN" &
RADIXOX_PID=$!
wait_for_server

echo "[2/6] Load — populating ${KEY_MAX} keys (${LOAD_TIME}s)..."
run_load /tmp/rx_load.txt
RADIXOX_LOAD_OPS=$(round_ops "$(mt_col /tmp/rx_load.txt Sets 2)")
echo "  Load: $RADIXOX_LOAD_OPS SET/sec"

echo "  Caching phase (ratio 1:10, Gaussian, ${TEST_TIME}s)..."
run_caching /tmp/rx_cache.txt
extract_phase /tmp/rx_cache.txt RADIXOX_CACHE
echo "  Caching: ${RADIXOX_CACHE_OPS} ops/sec | GET avg ${RADIXOX_CACHE_GET_AVG}ms | GET p99 ${RADIXOX_CACHE_GET_P99}ms"

echo "  Session phase (ratio 1:1, TTL, ${TEST_TIME}s)..."
run_session /tmp/rx_sess.txt
extract_phase /tmp/rx_sess.txt RADIXOX_SESS
echo "  Session: ${RADIXOX_SESS_OPS} ops/sec | GET avg ${RADIXOX_SESS_GET_AVG}ms | SET p99 ${RADIXOX_SESS_SET_P99}ms"

RADIXOX_PEAK_MB=$(peak_rss_mb "$RADIXOX_PID")
echo "  Peak RSS: ${RADIXOX_PEAK_MB} MB"
echo ""
kill_port

# ============================================================
# Valkey
# ============================================================
if [ "$VALKEY_IO_THREADS" -gt 1 ]; then
  echo "[3/6] Starting Valkey MT (io-threads=$VALKEY_IO_THREADS, cores ${VALKEY_MT_FIRST}-${VALKEY_MT_LAST}) on :$PORT..."
  taskset -c "${VALKEY_MT_FIRST}-${VALKEY_MT_LAST}" valkey-server \
    --port "$PORT" --daemonize no --save "" --appendonly no --dir /tmp \
    --io-threads "$VALKEY_IO_THREADS" --io-threads-do-reads yes &
else
  echo "[3/6] Starting Valkey (single-thread) on :$PORT..."
  taskset -c 0-1 valkey-server \
    --port "$PORT" --daemonize no --save "" --appendonly no --dir /tmp &
fi
VALKEY_PID=$!
wait_for_server

echo "[4/6] Load — populating ${KEY_MAX} keys (${LOAD_TIME}s)..."
run_load /tmp/vk_load.txt
VALKEY_LOAD_OPS=$(round_ops "$(mt_col /tmp/vk_load.txt Sets 2)")
echo "  Load: $VALKEY_LOAD_OPS SET/sec"

echo "  Caching phase (ratio 1:10, Gaussian, ${TEST_TIME}s)..."
run_caching /tmp/vk_cache.txt
extract_phase /tmp/vk_cache.txt VALKEY_CACHE
echo "  Caching: ${VALKEY_CACHE_OPS} ops/sec | GET avg ${VALKEY_CACHE_GET_AVG}ms | GET p99 ${VALKEY_CACHE_GET_P99}ms"

echo "  Session phase (ratio 1:1, TTL, ${TEST_TIME}s)..."
run_session /tmp/vk_sess.txt
extract_phase /tmp/vk_sess.txt VALKEY_SESS
echo "  Session: ${VALKEY_SESS_OPS} ops/sec | GET avg ${VALKEY_SESS_GET_AVG}ms | SET p99 ${VALKEY_SESS_SET_P99}ms"

VALKEY_PEAK_MB=$(peak_rss_mb "$VALKEY_PID")
echo "  Peak RSS: ${VALKEY_PEAK_MB} MB"
echo ""
kill_port

# ============================================================
# Tableau comparatif
# ============================================================
winner() {
  local a=$1 b=$2 higher=$3
  [ -z "$a" ] || [ -z "$b" ] || [ "$a" = "N/A" ] || [ "$b" = "N/A" ] && echo "N/A" && return
  if [ "$higher" = "1" ]; then
    echo "$a $b" | awk '{print ($1 > $2) ? "RadixOx ✓" : "Valkey  ✓"}'
  else
    echo "$a $b" | awk '{print ($1 < $2) ? "RadixOx ✓" : "Valkey  ✓"}'
  fi
}

VALKEY_LABEL="Valkey(1t)"
[ "$VALKEY_IO_THREADS" -gt 1 ] && VALKEY_LABEL="Valkey(${VALKEY_IO_THREADS}t)"

echo "═══════════════════════════════════════════════════════════════════"
echo "  RESULTS — memtier_benchmark 2.1 | latences en ms"
echo "═══════════════════════════════════════════════════════════════════"
printf "%-34s %12s %12s %12s\n" "Metric" "RadixOx(1t)" "$VALKEY_LABEL" "Winner"
echo "───────────────────────────────────────────────────────────────────"

echo ""
echo "  ── Load (pure SET, ${LOAD_TIME}s) ──────────────────────────────"
printf "%-34s %12s %12s %12s\n" "  Throughput (ops/sec)" \
  "$RADIXOX_LOAD_OPS" "$VALKEY_LOAD_OPS" \
  "$(winner "$RADIXOX_LOAD_OPS" "$VALKEY_LOAD_OPS" 1)"

echo ""
echo "  ── Caching (1:10 GET:SET, Gaussian, 100-500B) ───────────────────"
printf "%-34s %12s %12s %12s\n" "  Throughput (ops/sec)" \
  "$RADIXOX_CACHE_OPS" "$VALKEY_CACHE_OPS" \
  "$(winner "$RADIXOX_CACHE_OPS" "$VALKEY_CACHE_OPS" 1)"
printf "%-34s %12s %12s %12s\n" "  GET avg latency (ms)" \
  "$RADIXOX_CACHE_GET_AVG" "$VALKEY_CACHE_GET_AVG" \
  "$(winner "$RADIXOX_CACHE_GET_AVG" "$VALKEY_CACHE_GET_AVG" 0)"
printf "%-34s %12s %12s %12s\n" "  GET p99 latency (ms)" \
  "$RADIXOX_CACHE_GET_P99" "$VALKEY_CACHE_GET_P99" \
  "$(winner "$RADIXOX_CACHE_GET_P99" "$VALKEY_CACHE_GET_P99" 0)"
printf "%-34s %12s %12s %12s\n" "  GET p99.9 latency (ms)" \
  "$RADIXOX_CACHE_GET_P999" "$VALKEY_CACHE_GET_P999" \
  "$(winner "$RADIXOX_CACHE_GET_P999" "$VALKEY_CACHE_GET_P999" 0)"
printf "%-34s %12s %12s %12s\n" "  GET p99.99 latency (ms)" \
  "$RADIXOX_CACHE_GET_P9999" "$VALKEY_CACHE_GET_P9999" \
  "$(winner "$RADIXOX_CACHE_GET_P9999" "$VALKEY_CACHE_GET_P9999" 0)"
printf "%-34s %12s %12s %12s\n" "  SET p99 latency (ms)" \
  "$RADIXOX_CACHE_SET_P99" "$VALKEY_CACHE_SET_P99" \
  "$(winner "$RADIXOX_CACHE_SET_P99" "$VALKEY_CACHE_SET_P99" 0)"

echo ""
echo "  ── Session store (1:1, TTL 10-300s, 50-200B) ───────────────────"
printf "%-34s %12s %12s %12s\n" "  Throughput (ops/sec)" \
  "$RADIXOX_SESS_OPS" "$VALKEY_SESS_OPS" \
  "$(winner "$RADIXOX_SESS_OPS" "$VALKEY_SESS_OPS" 1)"
printf "%-34s %12s %12s %12s\n" "  GET avg latency (ms)" \
  "$RADIXOX_SESS_GET_AVG" "$VALKEY_SESS_GET_AVG" \
  "$(winner "$RADIXOX_SESS_GET_AVG" "$VALKEY_SESS_GET_AVG" 0)"
printf "%-34s %12s %12s %12s\n" "  GET p99 latency (ms)" \
  "$RADIXOX_SESS_GET_P99" "$VALKEY_SESS_GET_P99" \
  "$(winner "$RADIXOX_SESS_GET_P99" "$VALKEY_SESS_GET_P99" 0)"
printf "%-34s %12s %12s %12s\n" "  GET p99.9 latency (ms)" \
  "$RADIXOX_SESS_GET_P999" "$VALKEY_SESS_GET_P999" \
  "$(winner "$RADIXOX_SESS_GET_P999" "$VALKEY_SESS_GET_P999" 0)"
printf "%-34s %12s %12s %12s\n" "  SET p99 latency (ms)" \
  "$RADIXOX_SESS_SET_P99" "$VALKEY_SESS_SET_P99" \
  "$(winner "$RADIXOX_SESS_SET_P99" "$VALKEY_SESS_SET_P99" 0)"

echo ""
echo "  ── Mémoire ──────────────────────────────────────────────────────"
printf "%-34s %12s %12s %12s\n" "  Peak RSS (MB)" \
  "$RADIXOX_PEAK_MB" "$VALKEY_PEAK_MB" \
  "$(winner "$RADIXOX_PEAK_MB" "$VALKEY_PEAK_MB" 0)"

echo ""
echo "Logs complets : /tmp/{rx,vk}_{load,cache,sess}.txt"
