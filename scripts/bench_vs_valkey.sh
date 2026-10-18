#!/bin/bash
# RadixOx contre Valkey, un cœur chacun : memtier (1:4 SET:GET, sans
# pipeline et pipeline 32) puis YCSB workload A. Sert à la section
# Benchmarks du README.
#
#   scripts/bench_vs_valkey.sh                 # ~3 min
#   ROUNDS=1 TEST_TIME=5 scripts/bench_vs_valkey.sh   # essai rapide
#   VK_IO_THREADS=4 VK_CPUS=0,1,6,7 SKIP_YCSB=1 SKIP_BGSAVE=1 scripts/bench_vs_valkey.sh
#                                              # Valkey multi-thread sur 2 cœurs physiques
#
# - Les deux serveurs tournent en même temps, thread principal épinglé sur
#   le CPU 0 (le jumeau SMT, CPU 6, reste libre ; fils BGSAVE de Valkey sur
#   le CPU 1) ; un seul est sollicité à la fois et
#   l'ordre alterne à chaque round, pour répartir les dérives de la machine.
# - memtier sur les cœurs physiques 2–5, --distinct-client-seed --randomize.
# - CPU serveur (user + noyau) lu dans /proc pendant chaque run.
# - Phase BGSAVE : un run memtier sans pipeline par serveur, BGSAVE lancé à
#   t = 3 s ; durée du BGSAVE et latences du run (dump sur $OUT, tmpfs en
#   général : coût du disque exclu).
# - Résultats : médiane des rounds (et min–max) dans $OUT/summary.txt.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
YCSB_DIR="$(cd "$ROOT/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_vs_valkey.XXXX)}"
ROUNDS="${ROUNDS:-3}"
TEST_TIME="${TEST_TIME:-10}"
KEYS="${KEYS:-1000000}"
YCSB_RECORDS="${YCSB_RECORDS:-1000000}"
YCSB_OPS="${YCSB_OPS:-2000000}"
SERVER_CPU=0
VK_IO_THREADS="${VK_IO_THREADS:-1}"
# Valkey mono-thread : thread principal seul sur le CPU 0 ; le fils du BGSAVE
# et les threads d'arrière-plan (bio) sur le CPU 1, comme en production où le
# fork ne prend pas le cœur du serveur. RadixOx fait son BGSAVE dans son
# unique thread.
if [ -z "${VK_CPUS:-}" ]; then
  VK_CPUS="$SERVER_CPU,1"
  VK_EXTRA=(--server-cpulist "$SERVER_CPU" --bio-cpulist 1 --bgsave-cpulist 1 --aof-rewrite-cpulist 1)
else
  VK_EXTRA=()
fi
CLIENT_CPUS=2-5
RX_PORT=6501
VK_PORT=6502
mkdir -p "$OUT"

cargo build --release -q -p radixox --manifest-path "$ROOT/Cargo.toml"
RX_BIN="$ROOT/target/release/radixox"

pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

start_rx() {
  systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
    env RADIXOX_PORT=$RX_PORT RADIXOX_DIR="$OUT" taskset -c $SERVER_CPU "$RX_BIN" >"$OUT/rx.log" 2>&1 &
  pids+=($!)
}
start_vk() {
  systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
    taskset -c "$VK_CPUS" valkey-server --port $VK_PORT --bind 127.0.0.1 --save "" \
    --appendonly no --io-threads "$VK_IO_THREADS" --dir "$OUT" "${VK_EXTRA[@]}" >"$OUT/vk.log" 2>&1 &
  pids+=($!)
}
wait_port() {
  for _ in $(seq 50); do redis-cli -p "$1" PING >/dev/null 2>&1 && return; sleep 0.1; done
  echo "serveur absent sur $1" >&2; exit 1
}
server_pid() { ss -ltnp | grep ":$1 " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2; }
cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
rss_mb() { awk '/VmRSS/ {printf "%d", $2 / 1024}' "/proc/$1/status"; }

memtier() {  # port pipeline json [args…]
  local port=$1 pipe=$2 json=$3; shift 3
  taskset -c $CLIENT_CPUS memtier_benchmark -s 127.0.0.1 -p "$port" --protocol=redis \
    --threads 4 --clients 20 --pipeline "$pipe" -d 100 \
    --key-prefix 'key:' --key-minimum 1 --key-maximum "$KEYS" \
    --distinct-client-seed --randomize --hide-histogram --print-percentiles 50,99,99.9,99.99 \
    --json-out-file "$json" "$@" >/dev/null 2>&1
}

# ── memtier ──────────────────────────────────────────────────────────────────
start_rx; start_vk
wait_port $RX_PORT; wait_port $VK_PORT
RX_PID=$(server_pid $RX_PORT); VK_PID=$(server_pid $VK_PORT)
echo "RadixOx pid $RX_PID, Valkey pid $VK_PID, résultats dans $OUT"

for srv in rx vk; do
  port=$([ $srv = rx ] && echo $RX_PORT || echo $VK_PORT)
  echo "remplissage $srv ($KEYS clés)…"
  memtier "$port" 64 "$OUT/fill_$srv.json" --ratio 1:0 --key-pattern P:P --requests allkeys \
    --threads 1 --clients 1
  echo "  DBSIZE $(redis-cli -p "$port" DBSIZE), RSS $(rss_mb "$(server_pid "$port")") MB"
done

TICK=$(getconf CLK_TCK)
for round in $(seq "$ROUNDS"); do
  order="rx vk"; [ $((round % 2)) = 0 ] && order="vk rx"
  for pipe in 1 32; do
    for srv in $order; do
      port=$([ $srv = rx ] && echo $RX_PORT || echo $VK_PORT)
      pid=$([ $srv = rx ] && echo "$RX_PID" || echo "$VK_PID")
      c0=$(cpu_ticks "$pid")
      memtier "$port" "$pipe" "$OUT/run_${srv}_p${pipe}_r${round}.json" \
        --ratio 1:4 --key-pattern R:R --test-time "$TEST_TIME"
      c1=$(cpu_ticks "$pid")
      echo "$(( (c1 - c0) * 1000 / TICK ))" >"$OUT/cpu_${srv}_p${pipe}_r${round}.txt"
      echo "  round $round pipeline $pipe $srv fait"
    done
  done
done
# ── Latence pendant un BGSAVE ────────────────────────────────────────────────
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
if [ "${SKIP_BGSAVE:-0}" = 0 ]; then
  for srv in rx vk; do
    port=$([ $srv = rx ] && echo $RX_PORT || echo $VK_PORT)
    log="$OUT/$srv.log"
    done_re=$([ $srv = rx ] && echo 'écrits en' || echo 'Background saving terminated')
    memtier "$port" 1 "$OUT/bgsave_$srv.json" --ratio 1:4 --key-pattern R:R --test-time "$TEST_TIME" &
    mt=$!
    sleep 3
    t0=$(now_ms)
    redis-cli -p "$port" BGSAVE >"$OUT/bgsave_reply_$srv.txt"
    until grep -q "$done_re" "$log"; do sleep 0.005; done
    echo $(( $(now_ms) - t0 )) >"$OUT/bgsave_ms_$srv.txt"
    [ $srv = vk ] && redis-cli -p "$port" INFO stats | tr -d '\r' | awk -F: '/latest_fork_usec/ {print $2}' >"$OUT/bgsave_fork_us_vk.txt"
    wait $mt
    echo "  BGSAVE $srv fait ($(cat "$OUT/bgsave_ms_$srv.txt") ms)"
  done
fi
for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
pids=()
sleep 0.5

[ "${SKIP_YCSB:-0}" = 0 ] || { python3 "$ROOT/scripts/bench_vs_valkey_summary.py" "$OUT" "$ROUNDS" "$TEST_TIME" | tee "$OUT/summary.txt"; exit 0; }

# ── YCSB workload A ──────────────────────────────────────────────────────────
ycsb() {  # phase port out
  taskset -c 1-5,7-11 "$YCSB_DIR/bin/ycsb.sh" "$1" redis -s -P "$YCSB_DIR/workloads/workloada" \
    -p redis.host=127.0.0.1 -p redis.port="$2" \
    -p fieldcount=1 -p fieldnamekey=false -p fieldlength=100 \
    -p recordcount="$YCSB_RECORDS" -p operationcount="$YCSB_OPS" \
    -threads 100 -p hdrhistogram.percentiles=50,99,99.9,99.99 >"$3" 2>&1
}
for srv in rx vk; do
  if [ $srv = rx ]; then start_rx; port=$RX_PORT; else start_vk; port=$VK_PORT; fi
  wait_port $port
  pid=$(server_pid $port)
  echo "YCSB $srv : load…"; ycsb load $port "$OUT/ycsb_load_$srv.txt"
  c0=$(cpu_ticks "$pid")
  echo "YCSB $srv : run…"; ycsb run $port "$OUT/ycsb_run_$srv.txt"
  c1=$(cpu_ticks "$pid")
  echo "$(( (c1 - c0) * 1000 / TICK ))" >"$OUT/ycsb_cpu_$srv.txt"
  awk '/VmHWM/ {printf "%d", $2 / 1024}' "/proc/$pid/status" >"$OUT/ycsb_rss_$srv.txt"
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  pids=()
  sleep 0.5
done

python3 "$ROOT/scripts/bench_vs_valkey_summary.py" "$OUT" "$ROUNDS" "$TEST_TIME" | tee "$OUT/summary.txt"
