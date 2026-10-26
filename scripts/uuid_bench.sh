#!/bin/bash
# RadixOx rx-2s (boucle CPU 0, SQPOLL CPU 1) sous la charge de `uuidload` :
# chargement de KEYS clés puis un passage par mode de MODES. Débit compté
# côté serveur (delta de total_commands_processed).
#
#   scripts/uuid_bench.sh                            # UUID 1M, zipf + uniform, ~30 s
#   KEYS=15000000 MODES=uniform scripts/uuid_bench.sh
#   SHAPE=key EXTRA=--scramble scripts/uuid_bench.sh
#   PROF=scripts/uuid_prof_l2.sh MODES=uniform scripts/uuid_bench.sh   # profil pendant la mesure
#
# - RX : binaire (défaut target/profiling/radixox, recompilé ; RX_KEEP=1 pour ne pas).
# - PROF : script lancé pendant la mesure avec PID, OUT et DUR ; la mesure dure
#   alors DUR + 4 s pour qu'il tombe dedans.
# - Client sur les CPU hors des cœurs 0 et 1 (CLIENT_CPUS), 6 threads × 14 connexions.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEYS="${KEYS:-1000000}"
SHAPE="${SHAPE:-uuid}"
MODES="${MODES:-zipf uniform}"
TIME="${TIME:-8}"
EXTRA="${EXTRA:-}"
PORT="${PORT:-6441}"
MEM_MAX="${MEM_MAX:-6G}"
CLIENT_CPUS="${CLIENT_CPUS:-2-5,8-11}"
RX="${RX:-$ROOT/target/profiling/radixox}"
OUT="${OUT:-$(mktemp -d /tmp/uuid_bench.XXXX)}"
DUR="${DUR:-5}"

[ -n "${RX_KEEP:-}" ] || cargo build -q --profile profiling -p radixox --manifest-path "$ROOT/Cargo.toml"
cargo build -q --release --manifest-path "$ROOT/scripts/uuidload/Cargo.toml"
LOAD="$ROOT/scripts/uuidload/target/release/uuidload"

echo "charge des CPU (doit être faible) : $(cut -d' ' -f1-3 /proc/loadavg)"
dir="$(mktemp -d)"
RADIXOX_CPU_PIN=0 RADIXOX_SQPOLL_CPU=1 RADIXOX_PORT="$PORT" RADIXOX_SAVE="" RADIXOX_DIR="$dir" \
  systemd-run --user --scope -q -p MemoryMax="$MEM_MAX" "$RX" >"$OUT/server.log" 2>&1 &
for _ in $(seq 50); do redis-cli -p "$PORT" ping >/dev/null 2>&1 && break; sleep 0.1; done
PID="$(pgrep -nf "^$RX")"
trap 'kill "$PID" 2>/dev/null || true; rm -rf "$dir"' EXIT

run() { taskset -c "$CLIENT_CPUS" "$LOAD" --port "$PORT" --keys "$KEYS" --shape "$SHAPE" $EXTRA "$@"; }

echo -n "load     : "; run --mode load
echo "RSS $(awk '/VmRSS/ {print int($2/1024)}' /proc/$PID/status) Mo"
for m in $MODES; do
  t="$TIME" prof=
  if [ -n "${PROF:-}" ]; then
    t=$((DUR + 4))
    mkdir -p "$OUT/$m"
    (sleep 2; PID="$PID" OUT="$OUT/$m" DUR="$DUR" "$PROF") &
    prof=$!
  fi
  printf '%-9s: ' "$m"; run --mode "$m" --time "$t"
  [ -z "$prof" ] || wait "$prof"
done
echo "résultats : $OUT"
