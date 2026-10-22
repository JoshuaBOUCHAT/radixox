#!/bin/bash
set -euo pipefail

# Usage:
#   ./bench_compare.sh                    # RadixOx vs Valkey (starts/stops both, as before)
#   SERVER_CORES=2 ./bench_compare.sh     # K physical cores for each server, the rest for YCSB.
#                                         # One thread per physical core, same CPUs for both:
#                                         #   RadixOx: event loop on core 0, one io-wq worker
#                                         #   pinned on each of the other K-1 cores;
#                                         #   Valkey: io-threads=K (main + K-1).
#                                         # The SMT siblings of the server cores stay idle.
#   ./bench_compare.sh --running [label]  # bench whatever server is ALREADY running on $PORT
#                                          # (any Redis-compatible service). Does not start or
#                                          # kill anything. label defaults to "Service".
#   RECORDS=1000000 OPS=2000000 ./bench_compare.sh   # smaller run (defaults 5M / 10M)
#   RADIXOX_* variables of the calling shell reach RadixOx (e.g. RADIXOX_MIN_WAIT_US=10).
#   WARMUP=2: run latencies leave out the first WARMUP seconds (JVM warm-up of YCSB).
#   CPU rows: /proc/stat on each server's own CPUs during each phase (softirq included):
#   ops/s per 100% CPU (= ops / busy CPU-seconds), CPUs busy, user/kernel/idle %.
#
# Durability (same settings for both servers; default: none, as before):
#   AOF=everysec ./bench_compare.sh       # AOF: off | no | everysec | always | continuous
#                                         # (continuous is RadixOx only: Valkey gets everysec)
#   SAVE="60 10000" ./bench_compare.sh    # snapshot rules, Valkey's `save` syntax
#                                         # (RADIXOX_SAVE / --save); "" = none
#   AOF=everysec SAVE="3600 1 300 100 60 10000" ./bench_compare.sh   # Valkey's defaults
#   DIR=~/aofbench/compare                # where the files go: a real disk, never tmpfs
#   With AOF or SAVE on, one more physical core is taken from YCSB for background work,
#   given to both: Valkey's fork children and bio threads (bgsave/aof-rewrite/bio
#   cpulists), RadixOx's file writes and fsyncs (RADIXOX_FILE_CPUS). Auto-rewrite of the
#   log: 100 % / 64mb on both. No save at shutdown on either (outside the measurement).
AOF="${AOF:-off}"
SAVE="${SAVE-}"
case "$AOF" in
  off | no | everysec | always | continuous) ;;
  *) echo "AOF=$AOF : off | no | everysec | always | continuous" >&2; exit 1 ;;
esac
PERSIST=0
{ [ "$AOF" != off ] || [ -n "$SAVE" ]; } && PERSIST=1
DIR="${DIR:-$HOME/aofbench/compare}"
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
RECORDS="${RECORDS:-5000000}"
OPS="${OPS:-10000000}"
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
if [ "$SERVER_CORES" -lt 1 ] || [ "$((SERVER_CORES + PERSIST))" -ge "$NCORES" ]; then
  echo "SERVER_CORES=$SERVER_CORES : entre 1 et $((NCORES - 1 - PERSIST)) ($NCORES cœurs physiques$([ $PERSIST = 1 ] && echo ", dont un pour la persistance"))" >&2
  exit 1
fi
join() { local IFS=,; echo "$*"; }
# Un thread par cœur physique : le premier CPU de chaque cœur serveur ; ses frères SMT
# ne sont donnés à personne (ni serveur, ni client).
# Avec persistance, le cœur suivant (tous ses CPU logiques) va aux tâches de fond.
SERVER_SET=(); IOWQ_SET=(); CLIENT_SET=(); IDLE_SET=(); BG_CPUS=""
for i in "${!CORE_IDS[@]}"; do
  cpus="${CORE_CPUS[${CORE_IDS[$i]}]}"
  first="${cpus%%,*}"
  if [ "$i" -lt "$SERVER_CORES" ]; then
    SERVER_SET+=("$first")
    [ "$i" -gt 0 ] && IOWQ_SET+=("$first")
    [ "$cpus" != "$first" ] && IDLE_SET+=("${cpus#*,}")
  elif [ "$i" -eq "$SERVER_CORES" ] && [ "$PERSIST" = 1 ]; then
    BG_CPUS="$cpus"
  else
    CLIENT_SET+=("$cpus")
  fi
done
SERVER_CPUS=$(join "${SERVER_SET[@]}")
MAIN_CPU="${SERVER_SET[0]}"
IOWQ_CPUS=$(join "${IOWQ_SET[@]+"${IOWQ_SET[@]}"}")
IDLE_CPUS=$(join "${IDLE_SET[@]+"${IDLE_SET[@]}"}")
CLIENT_CPUS=$(join "${CLIENT_SET[@]}")
IOWQ_WORKERS=$((SERVER_CORES - 1))
# Valkey : thread principal + un io-thread par autre cœur, sur les mêmes CPU que RadixOx.
VALKEY_IO_THREADS="${VALKEY_IO_THREADS:-$SERVER_CORES}"
VALKEY_CPUS="$SERVER_CPUS"

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

# Dossier propre pour un serveur (sous-dossier de $DIR, refusé en tmpfs avec persistance)
fresh_dir() {
  local d="$DIR/$1"
  mkdir -p "$DIR"
  if [ "$PERSIST" = 1 ] && [ "$(findmnt -n -o FSTYPE -T "$DIR" 2>/dev/null || true)" = tmpfs ]; then
    echo "DIR=$DIR est en tmpfs (fsync gratuit, mesures fausses) : refusé" >&2
    exit 1
  fi
  rm -rf "$d"
  mkdir -p "$d"
  echo "$d"
}

# Ce que la persistance a fait pendant le bench (champs INFO présents sur le serveur)
persist_report() {
  local out=$1
  {
    redis-cli -p "$PORT" INFO persistence
    redis-cli -p "$PORT" INFO stats
  } 2>/dev/null | tr -d '\r' |
    grep -E '^(rdb_saves|rdb_last_bgsave_status|rdb_last_cow_size|aof_rewrites|aof_last_bgrewrite_status|aof_last_write_status|aof_last_cow_size|aof_current_size|aof_base_size|aof_delayed_fsync|aof_fsyncs|current_cow_peak|total_forks|latest_fork_usec):' |
    awk '!seen[$0]++' >"$out" || true
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
      -p hdrhistogram.percentiles=95,99,99.9,99.99 \
      $extra_args \
      >"$outfile" 2>&1
  else
    # Histogrammes par seconde (HDR_DIR) : les centiles se recalculent sans la chauffe.
    rm -rf "$HDR_DIR"; mkdir -p "$HDR_DIR"
    taskset -c "$CLIENT_CPUS" "$YCSB_DIR/bin/ycsb.sh" run redis -s -P "$YCSB_DIR/$WORKLOAD" \
      -p redis.host=127.0.0.1 -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false \
      -p fieldlength="$FIELDLENGTH" \
      -p operationcount="$OPS" \
      -p recordcount="$RECORDS" \
      -threads "$THREADS" \
      -p hdrhistogram.percentiles=95,99,99.9,99.99 \
      -p status.interval=1 -p hdrhistogram.fileoutput=true -p hdrhistogram.output.path="$HDR_DIR/" \
      $extra_args \
      >"$outfile" 2>&1
  fi
}

parse_stat() {
  local file=$1
  local metric=$2
  grep -F "$metric" "$file" 2>/dev/null | awk '{print $3}' | head -1 || true
}

# Centiles d'une opération sur la phase run, sans ses WARMUP premières secondes (chauffe de
# la JVM de YCSB : JIT et connexions font ~1 000 requêtes lentes et le max de la première
# seconde, assez pour décider du p99.99). Rend « p95 p99 p99.9 p99.99 max » en µs.
WARMUP="${WARMUP:-2}"
HDR_JAR=$(ls "$YCSB_DIR"/lib/HdrHistogram-*.jar 2>/dev/null | head -1)
hdr_tail() {
  local op=$1 # READ, UPDATE
  java -cp "$HDR_JAR" org.HdrHistogram.HistogramLogProcessor -i "$HDR_DIR/$op.hdr" \
    -start "$WARMUP" -outputValueUnitRatio 1 -percentilesOutputTicksPerHalf 200 2>/dev/null |
    awk 'BEGIN { n = split("0.95 0.99 0.999 0.9999", t, " ") }
      $2 ~ /^[01]\.[0-9]+$/ { for (j = 1; j <= n; j++) if (v[j] == "" && $2 + 0 >= t[j] - 1e-12) v[j] = $1 }
      /#\[Max/ { gsub(/[=,]/, " "); m = $2 }
      END { printf "%.0f %.0f %.0f %.0f %.0f\n", v[1], v[2], v[3], v[4], m }'
}

# Client considéré saturé au-delà de ce % de ses CPU logiques : les frères SMT ne
# doublent pas la capacité, ~60-65 % des CPU logiques occupent déjà les cœurs physiques.
CLIENT_SATURATED="${CLIENT_SATURATED:-60}"

# ── CPU des cœurs serveur pendant une phase (/proc/stat, par CPU) ──────────
# Par CPU et non par processus : le softirq réseau qui tourne sur un cœur serveur
# n'est compté dans aucun processus, mais il occupe bien ce cœur.
# Une ligne par CPU du jeu : « id user noyau idle » en ticks (user = user+nice,
# noyau = system+irq+softirq, idle = idle+iowait).
cpu_snap() {
  awk -v set=",$1," '$1 ~ /^cpu[0-9]+$/ { id = substr($1, 4)
    if (index(set, "," id ",")) print id, $2 + $3, $4 + $7 + $8, $5 + $6 }' /proc/stat
}

# cpu_report <avant> <après> <nb d'ops> → « user% noyau% idle% cpus_occupés ops_par_cpu_s par_cpu »
# Pourcentages de la capacité des CPU du jeu ; ops_par_cpu_s = débit qu'aurait un
# CPU occupé à 100 % (ops / secondes-CPU occupées), indépendant du démarrage de YCSB.
cpu_report() {
  awk -v ops="$3" -v hz="$(getconf CLK_TCK)" '
    NR == FNR { u[$1] = $2; k[$1] = $3; i[$1] = $4; next }
    { du = $2 - u[$1]; dk = $3 - k[$1]; di = $4 - i[$1]
      U += du; K += dk; I += di
      per = per sprintf("%s%s:%.0f%%", (per == "" ? "" : " "), $1, (du + dk) * 100 / (du + dk + di)) }
    END { T = U + K + I; busy = (U + K) / hz
      printf "%.0f %.0f %.0f %.2f %.0f %s\n", U * 100 / T, K * 100 / T, I * 100 / T,
        (U + K) / T * NR / 2, (busy > 0 ? ops / busy : 0), per }' "$1" "$2"
}

# Exécute une phase YCSB en relevant le CPU des cœurs serveur (CPU_<phase>) et des
# cœurs du client (CLIENT_<phase>) : un client saturé plafonne le débit des deux serveurs.
run_ycsb_cpu() {
  local phase=$1 outfile=$2 cpus=$3 nops=$4 t
  t=$(mktemp -d)
  cpu_snap "$cpus" >"$t/s0"; cpu_snap "$CLIENT_CPUS" >"$t/c0"
  run_ycsb "$phase" "$outfile"
  cpu_snap "$cpus" >"$t/s1"; cpu_snap "$CLIENT_CPUS" >"$t/c1"
  printf -v "CPU_$phase" '%s' "$(cpu_report "$t/s0" "$t/s1" "$nops")"
  printf -v "CLIENT_$phase" '%s' "$(cpu_report "$t/c0" "$t/c1" "$nops")"
  rm -rf "$t"
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
  HDR_DIR=/tmp/service_hdr
  run_ycsb run /tmp/service_run.txt
  read -r SVC_READ_P95 SVC_READ_P99 SVC_READ_P999 SVC_READ_P9999 SVC_READ_MAX <<<"$(hdr_tail READ)"
  read -r _ SVC_UPDATE_P99 _ _ SVC_UPDATE_MAX <<<"$(hdr_tail UPDATE)"
  SVC_RUN_OPS=$(parse_stat /tmp/service_run.txt "Throughput(ops/sec)")
  SVC_READ_AVG=$(parse_stat /tmp/service_run.txt "[READ], AverageLatency")
  SVC_READ_MAX_ALL=$(parse_stat /tmp/service_run.txt "[READ], MaxLatency(us)")
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
  printf "%-28s %12s\n" "READ max (µs)" "$SVC_READ_MAX"
  printf "%-28s %12s\n" "UPDATE max (µs)" "$SVC_UPDATE_MAX"
  printf "%-28s %12s\n" "READ max incl. warm-up (µs)" "$SVC_READ_MAX_ALL"
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
echo "  Server: $SERVER_CORES physical core(s), one thread each: CPUs $SERVER_CPUS"
echo "    RadixOx: event loop on $MAIN_CPU${IOWQ_CPUS:+, io-wq send workers on $IOWQ_CPUS}"
echo "    Valkey:  io-threads=$VALKEY_IO_THREADS on $VALKEY_CPUS"
echo "  Idle (SMT siblings of server cores): ${IDLE_CPUS:-none} | YCSB: CPUs $CLIENT_CPUS"
VALKEY_AOF="$AOF"
[ "$AOF" = continuous ] && VALKEY_AOF=everysec
if [ "$PERSIST" = 1 ]; then
  echo "  Durability: AOF=$AOF$([ "$AOF" = continuous ] && echo " (Valkey: everysec)") | SAVE=\"$SAVE\""
  echo "    background CPUs (both): $BG_CPUS | files: $DIR ($(findmnt -n -o SOURCE,FSTYPE -T "$(dirname "$DIR")" 2>/dev/null || true))"
else
  echo "  Durability: none (no AOF, no snapshot)"
fi
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
RADIXOX_DIR=$(fresh_dir radixox)
RADIXOX_ENV=(RADIXOX_CPU_PIN="$MAIN_CPU" RADIXOX_PORT="$PORT" RADIXOX_DIR="$RADIXOX_DIR"
  RADIXOX_SAVE="$SAVE" RADIXOX_SAVE_ON_EXIT=no)
if [ "$IOWQ_WORKERS" -gt 0 ]; then
  RADIXOX_ENV+=(RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS="$IOWQ_WORKERS" RADIXOX_IOWQ_CPUS="$IOWQ_CPUS")
fi
RADIXOX_CPUS="$SERVER_CPUS"
if [ "$PERSIST" = 1 ]; then
  RADIXOX_ENV+=(RADIXOX_FILE_CPUS="$BG_CPUS" RADIXOX_AUTO_AOF_REWRITE_PERCENTAGE=100
    RADIXOX_AUTO_AOF_REWRITE_MIN_SIZE=64mb)
  RADIXOX_CPUS="$SERVER_CPUS,$BG_CPUS"
fi
if [ "$AOF" != off ]; then
  RADIXOX_ENV+=(RADIXOX_APPENDONLY=yes RADIXOX_APPENDFSYNC="$AOF")
fi
taskset -c "$RADIXOX_CPUS" env "${RADIXOX_ENV[@]}" "$RADIXOX_BIN" &
RADIXOX_PID=$!
wait_for_server

echo "[2/4] Benchmarking RadixOx..."
echo "  Loading $RECORDS records..."
CURRENT_LOG=/tmp/radixox_load.txt
run_ycsb_cpu load /tmp/radixox_load.txt "$RADIXOX_CPUS" "$RECORDS"
RADIXOX_CPU_LOAD=$CPU_load
RADIXOX_CLIENT_LOAD=$CLIENT_load
RADIXOX_LOAD_OPS=$(parse_stat /tmp/radixox_load.txt "Throughput(ops/sec)")
RADIXOX_LOAD_P99=$(parse_stat /tmp/radixox_load.txt "[INSERT], 99thPercentileLatency")
echo "  Load: $RADIXOX_LOAD_OPS ops/sec | P99: $RADIXOX_LOAD_P99 µs"

echo "  Running $OPS operations..."
CURRENT_LOG=/tmp/radixox_run.txt
HDR_DIR=/tmp/radixox_hdr
run_ycsb_cpu run /tmp/radixox_run.txt "$RADIXOX_CPUS" "$OPS"
RADIXOX_CPU_RUN=$CPU_run
RADIXOX_CLIENT_RUN=$CLIENT_run
RADIXOX_RUN_OPS=$(parse_stat /tmp/radixox_run.txt "Throughput(ops/sec)")
RADIXOX_READ_AVG=$(parse_stat /tmp/radixox_run.txt "[READ], AverageLatency")
read -r RADIXOX_READ_P95 RADIXOX_READ_P99 RADIXOX_READ_P999 RADIXOX_READ_P9999 RADIXOX_READ_MAX <<<"$(hdr_tail READ)"
read -r _ RADIXOX_UPDATE_P99 _ _ RADIXOX_UPDATE_MAX <<<"$(hdr_tail UPDATE)"
RADIXOX_READ_MAX_ALL=$(parse_stat /tmp/radixox_run.txt "[READ], MaxLatency(us)")
echo "  Run: $RADIXOX_RUN_OPS ops/sec"

# Read peak RSS before killing
RADIXOX_PEAK_MB=$(peak_rss_mb "$RADIXOX_PID")
echo "  Peak RSS: ${RADIXOX_PEAK_MB} MB"
persist_report /tmp/radixox_persist.txt
echo ""

kill_port

# ============================================
# Valkey Benchmark
# ============================================
echo "[3/4] Starting Valkey (io-threads=$VALKEY_IO_THREADS, CPUs $VALKEY_CPUS) on :$PORT..."
VALKEY_DIR=$(fresh_dir valkey)
VALKEY_ARGS=(--port "$PORT" --daemonize no --dir "$VALKEY_DIR" --save "$SAVE"
  --io-threads "$VALKEY_IO_THREADS" --server-cpulist "$VALKEY_CPUS"
  --shutdown-on-sigterm nosave --shutdown-on-sigint nosave)
VALKEY_TASKSET="$SERVER_CPUS"
if [ "$PERSIST" = 1 ]; then
  VALKEY_ARGS+=(--bio-cpulist "$BG_CPUS" --bgsave-cpulist "$BG_CPUS" --aof-rewrite-cpulist "$BG_CPUS"
    --auto-aof-rewrite-percentage 100 --auto-aof-rewrite-min-size 64mb)
  VALKEY_TASKSET="$SERVER_CPUS,$BG_CPUS"
fi
if [ "$AOF" = off ]; then
  VALKEY_ARGS+=(--appendonly no)
else
  VALKEY_ARGS+=(--appendonly yes --appendfsync "$VALKEY_AOF")
fi
taskset -c "$VALKEY_TASKSET" valkey-server "${VALKEY_ARGS[@]}" &
VALKEY_PID=$!
wait_for_server

echo "[4/4] Benchmarking Valkey..."
echo "  Loading $RECORDS records..."
CURRENT_LOG=/tmp/valkey_load.txt
run_ycsb_cpu load /tmp/valkey_load.txt "$VALKEY_TASKSET" "$RECORDS"
VALKEY_CPU_LOAD=$CPU_load
VALKEY_CLIENT_LOAD=$CLIENT_load
VALKEY_LOAD_OPS=$(parse_stat /tmp/valkey_load.txt "Throughput(ops/sec)")
VALKEY_LOAD_P99=$(parse_stat /tmp/valkey_load.txt "[INSERT], 99thPercentileLatency")
echo "  Load: $VALKEY_LOAD_OPS ops/sec | P99: $VALKEY_LOAD_P99 µs"

echo "  Running $OPS operations..."
CURRENT_LOG=/tmp/valkey_run.txt
HDR_DIR=/tmp/valkey_hdr
run_ycsb_cpu run /tmp/valkey_run.txt "$VALKEY_TASKSET" "$OPS"
VALKEY_CPU_RUN=$CPU_run
VALKEY_CLIENT_RUN=$CLIENT_run
VALKEY_RUN_OPS=$(parse_stat /tmp/valkey_run.txt "Throughput(ops/sec)")
VALKEY_READ_AVG=$(parse_stat /tmp/valkey_run.txt "[READ], AverageLatency")
read -r VALKEY_READ_P95 VALKEY_READ_P99 VALKEY_READ_P999 VALKEY_READ_P9999 VALKEY_READ_MAX <<<"$(hdr_tail READ)"
read -r _ VALKEY_UPDATE_P99 _ _ VALKEY_UPDATE_MAX <<<"$(hdr_tail UPDATE)"
VALKEY_READ_MAX_ALL=$(parse_stat /tmp/valkey_run.txt "[READ], MaxLatency(us)")
echo "  Run: $VALKEY_RUN_OPS ops/sec"

# Read peak RSS before killing
VALKEY_PEAK_MB=$(peak_rss_mb "$VALKEY_PID")
echo "  Peak RSS: ${VALKEY_PEAK_MB} MB"
persist_report /tmp/valkey_persist.txt
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

# Lignes CPU d'une phase : ops par seconde-CPU, cœurs occupés, user/noyau/idle.
cpu_rows() {
  local phase=$1 r v
  read -r -a r <<<"$2"; read -r -a v <<<"$3"
  printf "%-28s %12s %12s %10s\n" "$phase ops/s per 100% CPU" "${r[4]}" "${v[4]}" "$(winner "${r[4]}" "${v[4]}" 1)"
  printf "%-28s %12s %12s\n" "$phase CPUs busy" "${r[3]}" "${v[3]}"
  printf "%-28s %12s %12s\n" "$phase user/kernel/idle %" "${r[0]}/${r[1]}/${r[2]}" "${v[0]}/${v[1]}/${v[2]}"
  read -r -a r <<<"$4"; read -r -a v <<<"$5"
  printf "%-28s %12s %12s\n" "$phase YCSB client busy %" "$((r[0] + r[1]))" "$((v[0] + v[1]))"
  if [ "$((r[0] + r[1]))" -ge "$CLIENT_SATURATED" ] || [ "$((v[0] + v[1]))" -ge "$CLIENT_SATURATED" ]; then
    printf "  ⚠ %s : YCSB uses ≥ %s %% of its CPUs, the client may be the limit\n" "$phase" "$CLIENT_SATURATED"
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
cpu_rows Load "$RADIXOX_CPU_LOAD" "$VALKEY_CPU_LOAD" "$RADIXOX_CLIENT_LOAD" "$VALKEY_CLIENT_LOAD"
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
# Le p99.99 ne voit pas quelques gros gels rares (fork de Valkey, appel disque) : le max, si.
printf "%-28s %12s %12s %10s\n" "READ max (µs)" \
  "$RADIXOX_READ_MAX" "$VALKEY_READ_MAX" \
  "$(winner "$RADIXOX_READ_MAX" "$VALKEY_READ_MAX" 0)"
printf "%-28s %12s %12s %10s\n" "UPDATE max (µs)" \
  "$RADIXOX_UPDATE_MAX" "$VALKEY_UPDATE_MAX" \
  "$(winner "$RADIXOX_UPDATE_MAX" "$VALKEY_UPDATE_MAX" 0)"
printf "%-28s %12s %12s\n" "READ max incl. warm-up (µs)" "$RADIXOX_READ_MAX_ALL" "$VALKEY_READ_MAX_ALL"
cpu_rows Run "$RADIXOX_CPU_RUN" "$VALKEY_CPU_RUN" "$RADIXOX_CLIENT_RUN" "$VALKEY_CLIENT_RUN"
echo ""
printf "%-28s %12s %12s %10s\n" "Peak RSS (MB)" \
  "${RADIXOX_PEAK_MB}" "${VALKEY_PEAK_MB}" \
  "$(winner "$RADIXOX_PEAK_MB" "$VALKEY_PEAK_MB" 0)"
echo ""
if [ "$PERSIST" = 1 ]; then
  echo "Persistence during the bench (INFO):"
  echo "  RadixOx: $(tr '\n' ' ' </tmp/radixox_persist.txt)"
  echo "           files: $(du -sh "$DIR/radixox" 2>/dev/null | cut -f1) (log segments are preallocated)"
  echo "  Valkey:  $(tr '\n' ' ' </tmp/valkey_persist.txt)"
  echo "           files: $(du -sh "$DIR/valkey" 2>/dev/null | cut -f1)"
  echo "  Peak RSS is the server process only: Valkey's fork children (BGSAVE, rewrite) are"
  echo "  extra processes, their copy-on-write is current_cow_peak / *_last_cow_size."
  echo ""
fi
echo "Run latencies (p95 → max): whole run minus its first ${WARMUP} s (YCSB's JVM warm-up), from"
echo "  per-second histograms (/tmp/{radixox,valkey}_hdr). READ avg and load P99: whole phase."
echo "CPU: /proc/stat on each server's own CPUs (RadixOx $RADIXOX_CPUS, Valkey $VALKEY_TASKSET),"
echo "  softirq included (kernel), YCSB start-up included (idle). Busy per CPU:"
for x in "RadixOx load:$RADIXOX_CPU_LOAD" "RadixOx run:$RADIXOX_CPU_RUN" "Valkey load:$VALKEY_CPU_LOAD" "Valkey run:$VALKEY_CPU_RUN" \
  "YCSB (RadixOx) run:$RADIXOX_CLIENT_RUN" "YCSB (Valkey) run:$VALKEY_CLIENT_RUN"; do
  read -r -a f <<<"${x#*:}"
  printf "  %-20s %s\n" "${x%%:*}" "${f[*]:5}"
done
echo ""
echo "Full logs: /tmp/radixox_{load,run}.txt  /tmp/valkey_{load,run}.txt"
