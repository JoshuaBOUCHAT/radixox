#!/bin/bash
set -euo pipefail

# Usage:
#   ./bench_compare.sh                    # RadixOx vs Valkey (starts/stops both, as before)
#   SERVER_CORES=2 ./bench_compare.sh     # K physical cores for each server, the rest for YCSB:
#                                         #   RadixOx: event loop on the first CPU, sends on io-wq
#                                         #   workers (one per CPU of the other K-1 cores);
#                                         #   Valkey: io-threads on the same CPUs.
#   ./bench_compare.sh --running [label]  # bench whatever server is ALREADY running on $PORT
#                                          # (any Redis-compatible service). Does not start or
#                                          # kill anything. label defaults to "Service".
MODE="compare"
RUNNING_LABEL="Service"
if [ "${1:-}" = "--running" ]; then
  MODE="running"
  RUNNING_LABEL="${2:-Service}"
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"
PORT=6379
RECORDS=5000000
OPS=10000000
THREADS=100
FIELDLENGTH=100

RADIXOX_BIN="${RADIXOX_BIN:-$SCRIPT_DIR/target/release/radixox}"
CURRENT_LOG=""

# ── Répartition des cœurs (SERVER_CORES = cœurs physiques par serveur) ──────
SERVER_CORES="${SERVER_CORES:-1}"
# cœur physique -> CPU logiques (frères SMT), dans l'ordre des cœurs
declare -A CORE_CPUS=()
CORE_IDS=()
while IFS=, read -r cpu core; do
  [[ "$cpu" =~ ^# ]] && continue
  [ -z "${CORE_CPUS[$core]:-}" ] && CORE_IDS+=("$core")
  CORE_CPUS[$core]="${CORE_CPUS[$core]:+${CORE_CPUS[$core]},}$cpu"
done < <(lscpu -p=CPU,CORE)
NCORES=${#CORE_IDS[@]}
if [ "$SERVER_CORES" -lt 1 ] || [ "$SERVER_CORES" -ge "$NCORES" ]; then
  echo "SERVER_CORES=$SERVER_CORES : entre 1 et $((NCORES - 1)) ($NCORES cœurs physiques)" >&2
  exit 1
fi
join() { local IFS=,; echo "$*"; }
SERVER_SET=(); IOWQ_SET=(); CLIENT_SET=()
for i in "${!CORE_IDS[@]}"; do
  cpus="${CORE_CPUS[${CORE_IDS[$i]}]}"
  if [ "$i" -lt "$SERVER_CORES" ]; then
    SERVER_SET+=("$cpus")
    [ "$i" -gt 0 ] && IOWQ_SET+=("$cpus")
  else
    CLIENT_SET+=("$cpus")
  fi
done
SERVER_CPUS=$(join "${SERVER_SET[@]}")
MAIN_CPU="${CORE_CPUS[${CORE_IDS[0]}]%%,*}"
IOWQ_CPUS=$(join "${IOWQ_SET[@]+"${IOWQ_SET[@]}"}")
CLIENT_CPUS=$(join "${CLIENT_SET[@]}")
IOWQ_WORKERS=0
[ -n "$IOWQ_CPUS" ] && IOWQ_WORKERS=$(echo "$IOWQ_CPUS" | tr ',' '\n' | wc -l)
# Valkey : boucle principale + un io-thread par CPU de délégation (mêmes CPU que RadixOx)
VALKEY_IO_THREADS="${VALKEY_IO_THREADS:-$((1 + IOWQ_WORKERS))}"
VALKEY_CPUS="$MAIN_CPU${IOWQ_CPUS:+,$IOWQ_CPUS}"

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
  [ "$MODE" = "running" ] || kill_port 2>/dev/null || true
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
    for i in $(seq 1 20); do
      if ! ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT"; then
        return 0
      fi
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

# Read peak RSS (VmHWM) from /proc/<pid>/status, returns value in MB
peak_rss_mb() {
  local pid=$1
  awk '/VmHWM/ {printf "%.0f", $2/1024}' "/proc/$pid/status" 2>/dev/null || echo "N/A"
}

run_ycsb() {
  local phase=$1 # load or run
  local outfile=$2
  local extra_args="${3:-}"

  if [ "$phase" = "load" ]; then
    taskset -c "$CLIENT_CPUS" "$YCSB_DIR/bin/ycsb.sh" load redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host=127.0.0.1 -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p recordcount="$RECORDS" \
      -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 \
      $extra_args \
      >"$outfile" 2>&1
  else
    taskset -c "$CLIENT_CPUS" "$YCSB_DIR/bin/ycsb.sh" run redis -s -P "$YCSB_DIR/$WORKLOAD" \
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

parse_inline_pct() {
  local file=$1
  local op=$2  # READ, UPDATE, INSERT
  local pct=$3 # 99.9 or 99.99
  grep -oP "\[$op:[^\]]*\]" "$file" 2>/dev/null | tail -1 |
    grep -oP "${pct}=\K[0-9]+" || true
}

# ============================================
# Mode: bench whatever is already running on $PORT
# ============================================
if [ "$MODE" = "running" ]; then
  echo "═══════════════════════════════════════════════════"
  echo "  YCSB Benchmark: $RUNNING_LABEL (already running on :$PORT)"
  echo "  Workload A: 50% read / 50% update"
  echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS"
  echo "  Field length: $FIELDLENGTH bytes"
  echo "═══════════════════════════════════════════════════"
  echo ""

  wait_for_server

  SVC_PID=$(ss -tlnp "sport = :$PORT" 2>/dev/null | grep -oP '(?<=pid=)\d+' | head -1 || true)

  echo "[1/2] Loading $RECORDS records..."
  CURRENT_LOG=/tmp/service_load.txt
  run_ycsb load /tmp/service_load.txt
  SVC_LOAD_OPS=$(parse_stat /tmp/service_load.txt "Throughput(ops/sec)")
  SVC_LOAD_P99=$(parse_stat /tmp/service_load.txt "[INSERT], 99thPercentileLatency")
  echo "  Load: $SVC_LOAD_OPS ops/sec | P99: $SVC_LOAD_P99 µs"

  echo "[2/2] Running $OPS operations..."
  CURRENT_LOG=/tmp/service_run.txt
  run_ycsb run /tmp/service_run.txt
  SVC_RUN_OPS=$(parse_stat /tmp/service_run.txt "Throughput(ops/sec)")
  SVC_READ_AVG=$(parse_stat /tmp/service_run.txt "[READ], AverageLatency")
  SVC_READ_P95=$(parse_stat /tmp/service_run.txt "[READ], 95thPercentileLatency")
  SVC_READ_P99=$(parse_stat /tmp/service_run.txt "[READ], 99thPercentileLatency")
  SVC_READ_P999=$(parse_inline_pct /tmp/service_run.txt READ 99.9)
  SVC_READ_P9999=$(parse_inline_pct /tmp/service_run.txt READ 99.99)
  SVC_UPDATE_P99=$(parse_stat /tmp/service_run.txt "[UPDATE], 99thPercentileLatency")
  echo "  Run: $SVC_RUN_OPS ops/sec"

  if [ -n "$SVC_PID" ]; then
    SVC_PEAK_MB=$(peak_rss_mb "$SVC_PID")
    echo "  Peak RSS: ${SVC_PEAK_MB} MB"
  else
    SVC_PEAK_MB="N/A"
  fi
  echo ""

  echo "═══════════════════════════════════════════════════"
  echo "  RESULTS — $RUNNING_LABEL"
  echo "═══════════════════════════════════════════════════"
  printf "%-28s %12s\n" "Load throughput (ops/sec)" "$SVC_LOAD_OPS"
  printf "%-28s %12s\n" "Load P99 (µs)" "$SVC_LOAD_P99"
  echo ""
  printf "%-28s %12s\n" "Run throughput (ops/sec)" "$SVC_RUN_OPS"
  printf "%-28s %12s\n" "READ avg (µs)" "$SVC_READ_AVG"
  printf "%-28s %12s\n" "READ P95 (µs)" "$SVC_READ_P95"
  printf "%-28s %12s\n" "READ P99 (µs)" "$SVC_READ_P99"
  printf "%-28s %12s\n" "READ P99.9 (µs)" "$SVC_READ_P999"
  printf "%-28s %12s\n" "READ P99.99 (µs)" "$SVC_READ_P9999"
  printf "%-28s %12s\n" "UPDATE P99 (µs)" "$SVC_UPDATE_P99"
  echo ""
  printf "%-28s %12s\n" "Peak RSS (MB)" "$SVC_PEAK_MB"
  echo ""
  echo "Full logs: /tmp/service_{load,run}.txt"
  echo ""
  echo "NOTE: server left running, not stopped (it wasn't started by this script)."

  CURRENT_LOG=""
  trap - EXIT
  exit 0
fi

# ============================================
# Build RadixOx
# ============================================
echo "═══════════════════════════════════════════════════"
echo "  YCSB Benchmark: RadixOx vs Valkey"
echo "  Workload A: 50% read / 50% update"
echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS"
echo "  Field length: $FIELDLENGTH bytes"
echo "  Server: $SERVER_CORES core(s), CPUs $SERVER_CPUS (event loop on $MAIN_CPU${IOWQ_CPUS:+, io-wq / io-threads on $IOWQ_CPUS}) | YCSB: CPUs $CLIENT_CPUS"
echo "  Valkey io-threads: $VALKEY_IO_THREADS (CPUs $VALKEY_CPUS)"
echo "═══════════════════════════════════════════════════"
echo ""
echo "[0/4] Building RadixOx (release)..."
RUSTFLAGS="-C target-cpu=native" cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 |
  tail -3
echo "  Build done: $RADIXOX_BIN"
echo ""

# ============================================
# RadixOx Benchmark
# ============================================
echo "[1/4] Starting RadixOx on :$PORT..."
kill_port || true
RADIXOX_ENV=(RADIXOX_CPU_PIN="$MAIN_CPU")
if [ "$IOWQ_WORKERS" -gt 0 ]; then
  RADIXOX_ENV+=(RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS="$IOWQ_WORKERS" RADIXOX_IOWQ_CPUS="$IOWQ_CPUS")
fi
taskset -c "$SERVER_CPUS" env "${RADIXOX_ENV[@]}" "$RADIXOX_BIN" &
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

# Read peak RSS before killing
RADIXOX_PEAK_MB=$(peak_rss_mb "$RADIXOX_PID")
echo "  Peak RSS: ${RADIXOX_PEAK_MB} MB"
echo ""

kill_port

# ============================================
# Valkey Benchmark
# ============================================
echo "[3/4] Starting Valkey (io-threads=$VALKEY_IO_THREADS, CPUs $VALKEY_CPUS) on :$PORT..."
taskset -c "$SERVER_CPUS" valkey-server \
  --port "$PORT" --daemonize no --save "" --appendonly no --dir /tmp \
  --io-threads "$VALKEY_IO_THREADS" --server-cpulist "$VALKEY_CPUS" &
VALKEY_PID=$!
wait_for_server

echo "[4/4] Benchmarking Valkey..."
echo "  Loading $RECORDS records..."
CURRENT_LOG=/tmp/valkey_load.txt
run_ycsb load /tmp/valkey_load.txt
VALKEY_LOAD_OPS=$(parse_stat /tmp/valkey_load.txt "Throughput(ops/sec)")
VALKEY_LOAD_P99=$(parse_stat /tmp/valkey_load.txt "[INSERT], 99thPercentileLatency")
echo "  Load: $VALKEY_LOAD_OPS ops/sec | P99: $VALKEY_LOAD_P99 µs"

echo "  Running $OPS operations..."
CURRENT_LOG=/tmp/valkey_run.txt
run_ycsb run /tmp/valkey_run.txt
VALKEY_RUN_OPS=$(parse_stat /tmp/valkey_run.txt "Throughput(ops/sec)")
VALKEY_READ_AVG=$(parse_stat /tmp/valkey_run.txt "[READ], AverageLatency")
VALKEY_READ_P95=$(parse_stat /tmp/valkey_run.txt "[READ], 95thPercentileLatency")
VALKEY_READ_P99=$(parse_stat /tmp/valkey_run.txt "[READ], 99thPercentileLatency")
VALKEY_READ_P999=$(parse_inline_pct /tmp/valkey_run.txt READ 99.9)
VALKEY_READ_P9999=$(parse_inline_pct /tmp/valkey_run.txt READ 99.99)
VALKEY_UPDATE_P99=$(parse_stat /tmp/valkey_run.txt "[UPDATE], 99thPercentileLatency")
echo "  Run: $VALKEY_RUN_OPS ops/sec"

# Read peak RSS before killing
VALKEY_PEAK_MB=$(peak_rss_mb "$VALKEY_PID")
echo "  Peak RSS: ${VALKEY_PEAK_MB} MB"
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
    echo "$a $b" | awk '{print ($1 > $2) ? "RadixOx" : "Valkey"}'
  else
    echo "$a $b" | awk '{print ($1 < $2) ? "RadixOx" : "Valkey"}'
  fi
}

VALKEY_LABEL="Valkey"
[ "$VALKEY_IO_THREADS" -gt 1 ] && VALKEY_LABEL="Valkey(${VALKEY_IO_THREADS}t)"
RADIXOX_LABEL="RadixOx(${SERVER_CORES}c)"

echo "═══════════════════════════════════════════════════"
echo "  RESULTS COMPARISON"
echo "═══════════════════════════════════════════════════"
printf "%-28s %12s %12s %10s\n" "Metric" "$RADIXOX_LABEL" "$VALKEY_LABEL" "Winner"
echo "-----------------------------------------------------------"
printf "%-28s %12s %12s %10s\n" "Load throughput (ops/sec)" \
  "$RADIXOX_LOAD_OPS" "$VALKEY_LOAD_OPS" \
  "$(winner "$RADIXOX_LOAD_OPS" "$VALKEY_LOAD_OPS" 1)"
printf "%-28s %12s %12s %10s\n" "Load P99 (µs)" \
  "$RADIXOX_LOAD_P99" "$VALKEY_LOAD_P99" \
  "$(winner "$RADIXOX_LOAD_P99" "$VALKEY_LOAD_P99" 0)"
echo ""
printf "%-28s %12s %12s %10s\n" "Run throughput (ops/sec)" \
  "$RADIXOX_RUN_OPS" "$VALKEY_RUN_OPS" \
  "$(winner "$RADIXOX_RUN_OPS" "$VALKEY_RUN_OPS" 1)"
printf "%-28s %12s %12s %10s\n" "READ avg (µs)" \
  "$RADIXOX_READ_AVG" "$VALKEY_READ_AVG" \
  "$(winner "$RADIXOX_READ_AVG" "$VALKEY_READ_AVG" 0)"
printf "%-28s %12s %12s %10s\n" "READ P95 (µs)" \
  "$RADIXOX_READ_P95" "$VALKEY_READ_P95" \
  "$(winner "$RADIXOX_READ_P95" "$VALKEY_READ_P95" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99 (µs)" \
  "$RADIXOX_READ_P99" "$VALKEY_READ_P99" \
  "$(winner "$RADIXOX_READ_P99" "$VALKEY_READ_P99" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99.9 (µs)" \
  "$RADIXOX_READ_P999" "$VALKEY_READ_P999" \
  "$(winner "$RADIXOX_READ_P999" "$VALKEY_READ_P999" 0)"
printf "%-28s %12s %12s %10s\n" "READ P99.99 (µs)" \
  "$RADIXOX_READ_P9999" "$VALKEY_READ_P9999" \
  "$(winner "$RADIXOX_READ_P9999" "$VALKEY_READ_P9999" 0)"
printf "%-28s %12s %12s %10s\n" "UPDATE P99 (µs)" \
  "$RADIXOX_UPDATE_P99" "$VALKEY_UPDATE_P99" \
  "$(winner "$RADIXOX_UPDATE_P99" "$VALKEY_UPDATE_P99" 0)"
echo ""
printf "%-28s %12s %12s %10s\n" "Peak RSS (MB)" \
  "${RADIXOX_PEAK_MB}" "${VALKEY_PEAK_MB}" \
  "$(winner "$RADIXOX_PEAK_MB" "$VALKEY_PEAK_MB" 0)"
echo ""
echo "Full logs: /tmp/radixox_{load,run}.txt  /tmp/valkey_{load,run}.txt"
