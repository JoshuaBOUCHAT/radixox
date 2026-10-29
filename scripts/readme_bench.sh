#!/bin/bash
# Chiffres du README : RadixOx contre Valkey, une section par sujet.
#
#   DIR=~/aofbench scripts/readme_bench.sh                    # ~10 min, tout
#   DIR=~/aofbench SECTIONS="speed aof" scripts/readme_bench.sh
#   DIR=~/aofbench KEYS=100000 TEST_TIME=3 scripts/readme_bench.sh   # essai rapide
#
# Sections : speed (1 cœur sans pipeline / pipeline 32, 3 cœurs sans pipeline, SCAN
# par préfixe), persist (BGSAVE sous charge, redémarrage), aof (SET seul, everysec /
# always), maxmemory (borne stricte, clés gardées sous la même limite), evict (taux
# de succès en Zipf sous maxmemory), repl (synchro complète, débit avec un réplica),
# types (Hash, Set, ZSet, List, BRPOP : RSS après chargement, débit sans pipeline /
# pipeline 32).
#
# Placement (Ryzen 7600X, 6 cœurs, jumeau SMT de N = N+6) : serveur sur le CPU 0 ;
# 2e cœur serveur = CPU 1 (fichiers de RadixOx, fils/bio de Valkey, réplica) ;
# 3 cœurs = CPU 0–2 ; memtier sur 3–5 et 9–11. Un serveur à la fois, un passage par
# cas (ordre de grandeur). Dumps et AOF dans $DIR, qui doit être sur un vrai disque.
set -euo pipefail
export RADIXOX_SAVE=""   # pas de save automatique (Valkey : --save "")

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${DIR:?DIR=<dossier sur disque>}"
SECTIONS="${SECTIONS:-speed persist aof maxmemory evict repl types}"
KEYS="${KEYS:-1000000}"
TEST_TIME="${TEST_TIME:-8}"
RX_BIN="${RX_BIN:-$ROOT/target/release/radixox}"
RX_EVICT_BIN="${RX_EVICT_BIN:-$ROOT/target/release-evict/radixox}"
OUT="${OUT:-$DIR/results}"
CLIENT_CPUS=3-5,9-11
P1=6601
P2=6602
[ "$(findmnt -n -o FSTYPE -T "$DIR" 2>/dev/null || true)" != tmpfs ] || { echo "$DIR en tmpfs : refusé" >&2; exit 1; }
mkdir -p "$DIR" "$OUT"
SUM="$OUT/summary.txt"
: >"$SUM"

if [ -z "${RX_BIN_KEEP:-}" ]; then
  cargo build -q --release -p radixox --manifest-path "$ROOT/Cargo.toml"
  cargo build -q --release -p radixox --features eviction --manifest-path "$ROOT/Cargo.toml" \
    --target-dir "$ROOT/target/evict"
  mkdir -p "$(dirname "$RX_EVICT_BIN")"
  cp "$ROOT/target/evict/release/radixox" "$RX_EVICT_BIN"
fi

pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
say() { echo "$*" | tee -a "$SUM"; }

# serveur : nom port cpus commande… (sous plafond mémoire)
start() {
  local name=$1 port=$2; shift 2
  systemd-run --user --scope -q -p MemoryMax=6G -p MemorySwapMax=0 "$@" >"$OUT/$name.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 100); do redis-cli -p "$port" PING >/dev/null 2>&1 && return; sleep 0.05; done
  echo "$name absent sur $port" >&2; exit 1
}
stop_all() { cleanup; pids=(); sleep 0.5; }
spid() { ss -ltnp | grep ":$1 " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2; }
rss() { awk '/VmRSS/ {printf "%d", $2 / 1024}' "/proc/$(spid "$1")/status"; }
hwm() { awk '/VmHWM/ {printf "%d", $2 / 1024}' "/proc/$(spid "$1")/status"; }
info() { redis-cli -p "$1" INFO | tr -d '\r' | awk -F: -v k="$2" '$1 == k {print $2}'; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

# RadixOx : port [VAR=val…] ; 1 cœur sauf RADIXOX_IOWQ_CPUS / FILE_CPUS donnés
rx() { local port=$1; shift; start "rx$port" "$port" env RADIXOX_PORT="$port" RADIXOX_DIR="$DIR/rx$port" RADIXOX_CPU_PIN="${CPU:-0}" "$@" "${BIN:-$RX_BIN}"; }
# Valkey : port cpus [options…]
vk() {
  local port=$1 cpus=$2; shift 2
  start "vk$port" "$port" taskset -c "$cpus" valkey-server --port "$port" --bind 127.0.0.1 --save "" \
    --appendonly no --repl-diskless-sync-delay 0 --dir "$DIR/vk$port" --enable-debug-command local "$@"
}
fresh() { rm -rf "$DIR/rx$1" "$DIR/vk$1"; mkdir -p "$DIR/rx$1" "$DIR/vk$1"; }
VK1=(--server-cpulist 0 --bio-cpulist 1 --bgsave-cpulist 1 --aof-rewrite-cpulist 1)

# memtier : port pipeline [args…] ; texte brut sur stdout
mt() {
  local port=$1 pipe=$2; shift 2
  taskset -c $CLIENT_CPUS memtier_benchmark -s 127.0.0.1 -p "$port" --protocol=redis \
    --threads 6 --clients 10 --pipeline "$pipe" -d 100 --key-prefix 'key:' \
    --distinct-client-seed --randomize --hide-histogram --print-percentiles 50,99,99.9,99.99 "$@" 2>/dev/null
}
# ligne memtier → « ops/s p50 p99 p99.9 p99.99 »
row() { awk -v t="$1" '$1 == t {printf "%9.0f ops/s  p50 %s  p99 %s  p99.9 %s  p99.99 %s ms", $2, $6, $7, $8, $9}'; }
fill() {  # port nb_clés
  mt "$1" 64 --threads 1 --clients 1 --ratio 1:0 --key-pattern P:P --key-minimum 1 \
    --key-maximum "$2" --requests allkeys >/dev/null
}
mixed() { mt "$1" "$2" --ratio 1:4 --key-pattern R:R --key-minimum 1 --key-maximum "$KEYS" --test-time "$TEST_TIME" "${@:3}"; }

# ── speed ────────────────────────────────────────────────────────────────────
sec_speed() {
  say "== speed : $KEYS clés de 100 o, 1:4 SET:GET, 60 connexions"
  for srv in rx vk; do
    fresh $P1
    if [ $srv = rx ]; then rx $P1; else vk $P1 0,1 "${VK1[@]}"; fi
    local t0; t0=$(now_ms); fill $P1 "$KEYS"
    say "$srv chargement $(( $(now_ms) - t0 )) ms, RSS $(rss $P1) Mo"
    say "$srv 1 cœur pipeline 1  $(mixed $P1 1 | row Totals)"
    say "$srv 1 cœur pipeline 32 $(mixed $P1 32 | row Totals)"
    # SCAN par préfixe : key:1234* = 1 111 clés parmi 1M
    t0=$(now_ms); local n; n=$(redis-cli -p $P1 --scan --pattern 'key:1234*' --count 1000 | wc -l)
    say "$srv SCAN MATCH key:1234* COUNT 1000 : $n clés en $(( $(now_ms) - t0 )) ms (client compris)"
    stop_all
  done
  say "-- 3 cœurs (CPU 0–2), sans pipeline"
  fresh $P1; rx $P1 RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS=2 RADIXOX_IOWQ_CPUS=1,2; fill $P1 "$KEYS"
  say "rx 3 cœurs pipeline 1 $(mixed $P1 1 | row Totals)"; stop_all
  fresh $P1; vk $P1 0-2 --io-threads 3 --server-cpulist 0-2; fill $P1 "$KEYS"
  say "vk 3 cœurs pipeline 1 $(mixed $P1 1 | row Totals)"; stop_all
}

# ── persist ──────────────────────────────────────────────────────────────────
wait_save() { while [ "$(info "$1" rdb_bgsave_in_progress)" != 0 ]; do sleep 0.005; done; }
sec_persist() {
  say "== persist : BGSAVE à t = 3 s sous charge (1 cœur, sans pipeline), dump sur disque"
  for srv in rx vk; do
    fresh $P1
    if [ $srv = rx ]; then rx $P1 RADIXOX_SAVE_ZSTD=0 RADIXOX_FILE_CPUS=1; else vk $P1 0,1 "${VK1[@]}"; fi
    fill $P1 "$KEYS"
    say "$srv sans save   $(mixed $P1 1 | row Totals)"
    mixed $P1 1 >"$OUT/persist_$srv.txt" &
    local m=$!; sleep 3
    local t0; t0=$(now_ms); redis-cli -p $P1 BGSAVE >/dev/null; sleep 0.01; wait_save $P1
    local ms=$(( $(now_ms) - t0 )); wait $m
    say "$srv pendant save $(row Totals <"$OUT/persist_$srv.txt")  BGSAVE $ms ms, pic RSS $(hwm $P1) Mo"
    redis-cli -p $P1 SAVE >/dev/null
    local f; f=$(ls "$DIR/$srv$P1"/dump.* | head -1)
    stop_all
    sync; t0=$(now_ms)
    if [ $srv = rx ]; then rx $P1; else vk $P1 0,1 "${VK1[@]}" --dbfilename "$(basename "$f")"; fi
    while [ "$(redis-cli -p $P1 DBSIZE 2>/dev/null)" != "$KEYS" ]; do sleep 0.01; done
    say "$srv redémarrage avec $KEYS clés : $(( $(now_ms) - t0 )) ms (dump $(( $(stat -c %s "$f") / 1048576 )) Mo)"
    stop_all
  done
}

# ── aof ──────────────────────────────────────────────────────────────────────
sec_aof() {
  say "== aof : SET seul, 100 o, AOF sur disque, 2e cœur pour les fichiers"
  local sets=(--ratio 1:0 --key-pattern R:R --key-minimum 1 --key-maximum "$KEYS" --test-time "$TEST_TIME")
  for c in "rx off" "rx everysec" "vk everysec" "rx always" "vk always"; do
    set -- $c; fresh $P1
    case "$1 $2" in
      "rx off") rx $P1 ;;
      rx*) rx $P1 RADIXOX_APPENDONLY=yes RADIXOX_APPENDFSYNC="$2" RADIXOX_FILE_CPUS=1 ;;
      vk*) vk $P1 0,1 "${VK1[@]}" --appendonly yes --appendfsync "$2" ;;
    esac
    say "$1 $2 pipeline 1  $(mt $P1 1 "${sets[@]}" | row Sets)"
    say "$1 $2 pipeline 16 $(mt $P1 16 "${sets[@]}" | row Sets)"
    stop_all
  done
}

# ── maxmemory ────────────────────────────────────────────────────────────────
MAXMEM="${MAXMEM:-200mb}"
sec_maxmemory() {
  local n=$((KEYS * 3))
  say "== maxmemory $MAXMEM : écriture de $n clés de 100 o (pipeline 64), pic de RSS du processus"
  for pol in noeviction allkeys-lru; do
    for srv in rx vk; do
      fresh $P1
      if [ $srv = rx ]; then BIN=$RX_EVICT_BIN rx $P1 RADIXOX_MAXMEMORY=$MAXMEM RADIXOX_MAXMEMORY_POLICY=$pol
      else vk $P1 0,1 "${VK1[@]}" --maxmemory $MAXMEM --maxmemory-policy $pol; fi
      fill $P1 "$n"
      sleep 1
      say "$srv $pol : $(redis-cli -p $P1 DBSIZE) clés gardées, RSS $(rss $P1) Mo, pic $(hwm $P1) Mo, SET suivant : $(redis-cli -p $P1 SET encore x | head -c 60)"
      stop_all
    done
  done
}

# ── evict ────────────────────────────────────────────────────────────────────
# Limites choisies pour une même RSS finale (~42 Mo) : celle de RadixOx compte tout le
# processus, celle de Valkey ses données seulement.
EVICT_MEM="${EVICT_MEM:-48mb}"
VK_EVICT_MEM="${VK_EVICT_MEM:-24mb}"
sec_evict() {
  local n=$((KEYS * 3))
  say "== evict : maxmemory $EVICT_MEM (Valkey $VK_EVICT_MEM), Zipf 0,99 sur $n clés, 1:4 SET:GET, sans pipeline (chauffe 6 s puis mesure)"
  local z=(--ratio 1:4 --key-pattern Z:Z --key-zipf-exp 0.99 --key-minimum 1 --key-maximum "$n")
  for c in "rx allkeys-random" "rx allkeys-lru" "rx allkeys-lfu" "vk allkeys-lru" "vk allkeys-lfu"; do
    set -- $c; fresh $P1
    if [ $1 = rx ]; then BIN=$RX_EVICT_BIN rx $P1 RADIXOX_MAXMEMORY=$EVICT_MEM RADIXOX_MAXMEMORY_POLICY=$2
    else vk $P1 0,1 "${VK1[@]}" --maxmemory $VK_EVICT_MEM --maxmemory-policy $2; fi
    mt $P1 1 "${z[@]}" --test-time 6 >/dev/null
    local o; o=$(mt $P1 1 "${z[@]}" --test-time "$TEST_TIME")
    say "$1 $2 : $(echo "$o" | awk '$1 == "Gets" {printf "succès %.1f %%", 100 * $3 / ($3 + $4)}')  $(echo "$o" | row Totals)  RSS $(rss $P1) Mo, $(redis-cli -p $P1 DBSIZE) clés, $(info $P1 evicted_keys) évincées"
    stop_all
  done
}

# ── repl ─────────────────────────────────────────────────────────────────────
sec_repl() {
  say "== repl : primaire CPU 0, réplica CPU 1, $KEYS clés"
  for srv in rx vk; do
    fresh $P1; fresh $P2
    if [ $srv = rx ]; then rx $P1; CPU=1 rx $P2
    else vk $P1 0 --server-cpulist 0; vk $P2 1 --server-cpulist 1; fi
    fill $P1 "$KEYS"
    local t0; t0=$(now_ms)
    if [ $srv = rx ]; then redis-cli -p $P1 REPLICATE 127.0.0.1 $P2 >/dev/null
    else redis-cli -p $P2 REPLICAOF 127.0.0.1 $P1 >/dev/null; fi
    while [ "$(redis-cli -p $P2 DBSIZE 2>/dev/null)" != "$KEYS" ]; do sleep 0.01; done
    say "$srv synchro complète : $(( $(now_ms) - t0 )) ms"
    say "$srv avec réplica pipeline 1  $(mixed $P1 1 | row Totals)"
    say "$srv avec réplica pipeline 32 $(mixed $P1 32 | row Totals)"
    local d1 d2; d1=$(redis-cli -p $P1 DEBUG DIGEST)
    for _ in $(seq 500); do d2=$(redis-cli -p $P2 DEBUG DIGEST); [ "$d1" = "$d2" ] && break; sleep 0.01; done
    say "$srv empreintes primaire / réplica après la charge : $([ "$d1" = "$d2" ] && echo identiques || echo DIFFÉRENTES)"
    stop_all
  done
}

# ── types ────────────────────────────────────────────────────────────────────
# cmd « commande » ratio motif → arguments memtier d'une commande arbitraire
cmd() { echo "--command=$1" "--command-ratio=$2" "--command-key-pattern=$3"; }
sec_types() {
  say "== types : 1 cœur, 60 connexions ; RSS = après chargement − à vide"
  local pre=(--key-prefix '' --key-minimum 1)
  for srv in rx vk; do
    fresh $P1
    if [ $srv = rx ]; then rx $P1; else vk $P1 0,1 "${VK1[@]}"; fi
    local base; base=$(rss $P1)
    # Hash : 1M clés à un champ de 100 o
    mt $P1 64 --threads 1 --clients 1 "${pre[@]}" --key-maximum "$KEYS" --requests allkeys \
      "--command=HSET h:__key__ f __data__" --command-key-pattern=P >/dev/null
    local r1; r1=$(rss $P1)
    local h=("${pre[@]}" --key-maximum "$KEYS" --test-time "$TEST_TIME"
      "--command=HSET h:__key__ f __data__" --command-ratio=1 --command-key-pattern=R
      "--command=HGET h:__key__ f" --command-ratio=4 --command-key-pattern=R)
    say "$srv Hash ($KEYS × 1 champ, +$((r1 - base)) Mo) HSET:HGET 1:4 p1  $(mt $P1 1 "${h[@]}" | row Totals)"
    say "$srv Hash HSET:HGET 1:4 p32 $(mt $P1 32 "${h[@]}" | row Totals)"
    # Set : un Set de 1M membres
    mt $P1 64 --threads 1 --clients 1 "${pre[@]}" --key-maximum "$KEYS" --requests allkeys \
      "--command=SADD s __key__" --command-key-pattern=P >/dev/null
    local r2; r2=$(rss $P1)
    local st=("${pre[@]}" --key-maximum $((KEYS * 2)) --test-time "$TEST_TIME"
      "--command=SADD s __key__" --command-ratio=1 --command-key-pattern=R
      "--command=SISMEMBER s __key__" --command-ratio=4 --command-key-pattern=R)
    say "$srv Set (1 × $KEYS membres, +$((r2 - r1)) Mo) SADD:SISMEMBER 1:4 p1  $(mt $P1 1 "${st[@]}" | row Totals)"
    say "$srv Set SADD:SISMEMBER 1:4 p32 $(mt $P1 32 "${st[@]}" | row Totals)"
    # ZSet : un ZSet de 1M membres, score = membre
    mt $P1 64 --threads 1 --clients 1 "${pre[@]}" --key-maximum "$KEYS" --requests allkeys \
      "--command=ZADD z __key__ __key__" --command-key-pattern=P >/dev/null
    local r3; r3=$(rss $P1)
    local z=("${pre[@]}" --key-maximum "$KEYS" --test-time "$TEST_TIME"
      "--command=ZADD z __key__ __key__" --command-ratio=1 --command-key-pattern=R
      "--command=ZSCORE z __key__" --command-ratio=2 --command-key-pattern=R
      "--command=ZRANK z __key__" --command-ratio=1 --command-key-pattern=R
      "--command=ZRANGEBYSCORE z __key__ +inf LIMIT 0 10" --command-ratio=1 --command-key-pattern=R)
    say "$srv ZSet (1 × $KEYS membres, +$((r3 - r2)) Mo) ZADD/ZSCORE/ZRANK/ZRANGEBYSCORE 1/2/1/1 p1  $(mt $P1 1 "${z[@]}" | row Totals)"
    say "$srv ZSet p32 $(mt $P1 32 "${z[@]}" | row Totals)"
    # List : 1 000 listes, 1M éléments de 100 o au total
    mt $P1 64 --threads 1 --clients 1 "${pre[@]}" --key-maximum 1000 --requests "$KEYS" \
      "--command=LPUSH l:__key__ __data__" --command-key-pattern=R >/dev/null
    local r4; r4=$(rss $P1)
    local l=("${pre[@]}" --key-maximum 1000 --test-time "$TEST_TIME"
      "--command=LPUSH l:__key__ __data__" --command-ratio=2 --command-key-pattern=R
      "--command=RPOP l:__key__" --command-ratio=2 --command-key-pattern=R
      "--command=LRANGE l:__key__ 0 9" --command-ratio=1 --command-key-pattern=R)
    say "$srv List (1000 × 1000 éléments, +$((r4 - r3)) Mo) LPUSH/RPOP/LRANGE 0 9 2/2/1 p1  $(mt $P1 1 "${l[@]}" | row Totals)"
    say "$srv List p32 $(mt $P1 32 "${l[@]}" | row Totals)"
    # Bloquantes : 60 consommateurs BRPOP en attente sur une file vide, 20 producteurs
    # LPUSH : moins de producteurs que de consommateurs, donc chaque élément part droit à
    # un client bloqué (la file restante doit rester près de 0)
    local q=("${pre[@]}" --key-maximum 1 --pipeline 1 -d 100
      --protocol=redis -s 127.0.0.1 -p $P1 --distinct-client-seed --randomize --hide-histogram
      --print-percentiles 50,99,99.9,99.99)
    taskset -c 5,9-11 memtier_benchmark "${q[@]}" --test-time $((TEST_TIME + 1)) --threads 4 --clients 15 \
      "--command=BRPOP q:__key__ 1" --command-key-pattern=R 2>/dev/null >"$OUT/cons_$srv.txt" &
    local cp=$!; sleep 0.3
    taskset -c 3-4 memtier_benchmark "${q[@]}" --test-time "$TEST_TIME" --threads 2 --clients 10 \
      "--command=LPUSH q:__key__ __data__" --command-key-pattern=R 2>/dev/null >"$OUT/prod_$srv.txt"
    wait $cp
    local left; left=$(redis-cli -p $P1 LLEN q:1)
    say "$srv BRPOP : LPUSH vers des clients bloqués $(row Totals <"$OUT/prod_$srv.txt") ; restés en file $left"
    stop_all
  done
}

echo "résultats : $SUM"
for s in $SECTIONS; do "sec_$s"; done
