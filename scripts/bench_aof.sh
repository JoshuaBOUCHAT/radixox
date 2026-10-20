#!/bin/bash
# AOF : RadixOx (off, everysec, always) contre Valkey (everysec, always), un
# cœur serveur chacun, memtier SET seul sans pipeline puis pipeline 16.
# Le dossier des fichiers DOIT être sur un vrai disque (pas tmpfs : fsync
# gratuit, mesures fausses).
#
#   DIR=~/aofbench scripts/bench_aof.sh          # ~2 min (TEST_TIME=8)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${DIR:?DIR=<dossier sur disque>}"
TEST_TIME="${TEST_TIME:-8}"
SERVER_CPU=0
CLIENT_CPUS=2-5
PORT=6503
if [ "$(findmnt -n -o FSTYPE -T "$DIR" 2>/dev/null || true)" = tmpfs ]; then
  echo "$DIR est en tmpfs : refusé" >&2; exit 1
fi
cargo build --release -q -p radixox --manifest-path "$ROOT/Cargo.toml"
RX_BIN="$ROOT/target/release/radixox"
pid=""
trap '[ -n "$pid" ] && kill $pid 2>/dev/null || true' EXIT

wait_port() {
  for _ in $(seq 100); do redis-cli -p $PORT PING >/dev/null 2>&1 && return; sleep 0.1; done
  echo "serveur absent" >&2; exit 1
}
run() {  # nom pipeline
  taskset -c $CLIENT_CPUS memtier_benchmark -s 127.0.0.1 -p $PORT --protocol=redis \
    --threads 4 --clients 10 --pipeline "$2" -d 100 --ratio 1:0 \
    --key-prefix 'key:' --key-minimum 1 --key-maximum 1000000 \
    --distinct-client-seed --randomize --hide-histogram --print-percentiles 50,99,99.9 \
    --test-time "$TEST_TIME" 2>/dev/null | awk -v n="$1 p$2" '/^Sets/ {printf "%-22s %10.0f ops/s  p50 %s  p99 %s  p99.9 %s ms\n", n, $2, $6, $7, $8}'
}
bench() {  # nom commande…
  local name=$1; shift
  rm -rf "$DIR"; mkdir -p "$DIR"
  systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 "$@" >"$DIR.log" 2>&1 &
  pid=$!
  wait_port
  run "$name" 1
  run "$name" 16
  kill $pid; wait $pid 2>/dev/null || true; pid=""
  sleep 0.5
}
rx() {  # nom fsync|off
  if [ "$2" = off ]; then
    bench "$1" env RADIXOX_SAVE= RADIXOX_PORT=$PORT RADIXOX_DIR="$DIR" taskset -c $SERVER_CPU "$RX_BIN"
  else
    bench "$1" env RADIXOX_SAVE= RADIXOX_PORT=$PORT RADIXOX_DIR="$DIR" RADIXOX_APPENDONLY=yes \
      RADIXOX_APPENDFSYNC="$2" RADIXOX_FILE_CPUS=1 RADIXOX_CPU_PIN=$SERVER_CPU taskset -c $SERVER_CPU,1 "$RX_BIN"
  fi
}
vk() {  # nom fsync
  bench "$1" taskset -c $SERVER_CPU,1 valkey-server --port $PORT --bind 127.0.0.1 --save "" \
    --appendonly yes --appendfsync "$2" --dir "$DIR" --server-cpulist $SERVER_CPU --bio-cpulist 1 \
    --aof-rewrite-cpulist 1
}
mkdir -p "$DIR"; echo "disque : $(findmnt -n -o SOURCE,FSTYPE -T "$DIR")"
rx "radixox aof off" off
rx "radixox everysec" everysec
vk "valkey everysec" everysec
rx "radixox always" always
vk "valkey always" always
