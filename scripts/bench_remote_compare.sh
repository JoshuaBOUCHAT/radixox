#!/bin/bash
# YCSB workload A contre RadixOx puis Valkey sur une machine distante, pilotée par ssh.
# À lancer sur la machine cliente, dans le dépôt :
#
#   scripts/bench_remote_compare.sh joshua@mini-pc        # 1 cœur par serveur
#   scripts/bench_remote_compare.sh joshua@mini-pc 3      # 3 cœurs physiques par serveur
#
# Le serveur distant doit avoir le dépôt (REMOTE_DIR, défaut ~/radixox) et
# `scripts/bench_server.sh prep` lancé depuis son dernier démarrage. Chaque serveur est
# démarré, chargé, mesuré puis arrêté par `scripts/bench_server.sh` (mêmes CPU pour les
# deux). Variables : RECORDS (5M), OPS (10M), THREADS (100), PORT (6379), HOST (adresse
# réseau du serveur, défaut : celle que ssh résout pour la cible, alias compris).
set -euo pipefail

TARGET="${1:?usage: $0 user@serveur [K]}"
K="${2:-1}"
# Adresse réseau du serveur : celle que ssh résout pour la cible (un alias de
# ~/.ssh/config comme « server » n'est connu ni de redis-cli ni de YCSB).
HOST="${HOST:-$(ssh -G "$TARGET" | awk '$1 == "hostname" {print $2; exit}')}"
PORT="${PORT:-6379}"
REMOTE_DIR="${REMOTE_DIR:-radixox}"
RECORDS="${RECORDS:-5000000}"
OPS="${OPS:-10000000}"
THREADS="${THREADS:-100}"
FIELDLENGTH=100
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_remote.XXXX)}"

remote() { ssh "$TARGET" "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh $*"; }
trap 'remote stop >/dev/null 2>&1 || true' EXIT

ycsb() { # phase fichier
  local phase=$1 out=$2 count
  if [ "$phase" = load ]; then count=(-p recordcount="$RECORDS"); else count=(-p operationcount="$OPS"); fi
  "$YCSB_DIR/bin/ycsb.sh" "$phase" redis -s -P "$YCSB_DIR/workloads/workloada" \
    -p redis.host="$HOST" -p redis.port="$PORT" \
    -p fieldcount=1 -p fieldnamekey=false -p fieldlength="$FIELDLENGTH" \
    -p recordcount="$RECORDS" "${count[@]}" -threads "$THREADS" \
    -p percentiles=95,99,99.9,99.99 >"$out" 2>&1
}
stat() { grep -F "$2" "$1" 2>/dev/null | awk '{print $3}' | head -1 || true; }
pct() { grep -oP "\[$2:[^\]]*\]" "$1" 2>/dev/null | tail -1 | grep -oP "$3=\K[0-9]+" || true; }

declare -A R
bench() { # serveur
  local s=$1
  echo "── $s ──"
  remote start "$s" "$K"
  redis-cli -h "$HOST" -p "$PORT" PING >/dev/null || { echo "injoignable depuis ce client : pare-feu ?"; exit 1; }
  echo "  load de $RECORDS enregistrements…"
  ycsb load "$OUT/${s}_load.txt"
  echo "  run de $OPS opérations…"
  ycsb run "$OUT/${s}_run.txt"
  R[$s,load]=$(stat "$OUT/${s}_load.txt" "Throughput(ops/sec)")
  R[$s,loadp99]=$(stat "$OUT/${s}_load.txt" "[INSERT], 99thPercentileLatency")
  R[$s,run]=$(stat "$OUT/${s}_run.txt" "Throughput(ops/sec)")
  R[$s,avg]=$(stat "$OUT/${s}_run.txt" "[READ], AverageLatency")
  R[$s,p95]=$(stat "$OUT/${s}_run.txt" "[READ], 95thPercentileLatency")
  R[$s,p99]=$(stat "$OUT/${s}_run.txt" "[READ], 99thPercentileLatency")
  R[$s,p999]=$(pct "$OUT/${s}_run.txt" READ 99.9)
  R[$s,p9999]=$(pct "$OUT/${s}_run.txt" READ 99.99)
  R[$s,up99]=$(stat "$OUT/${s}_run.txt" "[UPDATE], 99thPercentileLatency")
  R[$s,rss]=$(remote rss)
  remote stop
}

echo "YCSB A — $RECORDS enregistrements, $OPS opérations, $THREADS threads — serveur $HOST, $K cœur(s) chacun"
bench radixox
bench valkey

row() { # libellé clé plus_grand_gagne
  local a=${R[radixox,$2]} b=${R[valkey,$2]} w
  w=$(awk -v a="$a" -v b="$b" -v h="$3" 'BEGIN { if (a=="" || b=="") print "N/A"; else if ((h && a+0>b+0) || (!h && a+0<b+0)) print "RadixOx"; else print "Valkey" }')
  printf "%-26s %14s %14s %9s\n" "$1" "$a" "$b" "$w"
}
echo ""
printf "%-26s %14s %14s %9s\n" "" "RadixOx(${K}c)" "Valkey(${K}t)" "gagnant"
row "Load (ops/s)" load 1
row "Load p99 (µs)" loadp99 0
row "Run (ops/s)" run 1
row "READ avg (µs)" avg 0
row "READ p95 (µs)" p95 0
row "READ p99 (µs)" p99 0
row "READ p99.9 (µs)" p999 0
row "READ p99.99 (µs)" p9999 0
row "UPDATE p99 (µs)" up99 0
row "Pic de RSS (Mo)" rss 0
echo ""
echo "Logs : $OUT"
