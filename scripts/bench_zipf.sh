#!/bin/bash
# RadixOx contre Valkey en accès réaliste : clés tirées en Zipf (0,99 par défaut,
# quelques clés chaudes, une longue traîne), 1:4 SET:GET, 100 o. Chaque
# configuration serveur passe sans pipeline puis en pipeline 64 :
#
#   rx-Nc   RadixOx : boucle sur le 1er CPU de SERVER_CPUS, N−1 workers io-wq
#           (envois async) sur les suivants ; rx-1c = boucle seule
#   vk-Nt   Valkey io-threads N sur les N premiers CPU de SERVER_CPUS, bio sur
#           le suivant (ou le dernier)
#
#   scripts/bench_zipf.sh                         # ~2 min 30
#   TEST_TIME=5 PIPELINES=64 scripts/bench_zipf.sh   # essai rapide
#   CONFIGS="rx-1c vk-1t" ZIPF=1.2 scripts/bench_zipf.sh
#   CONFIGS="rx-3c vk-3t" scripts/bench_zipf.sh    # CPU 0, 1, 6
#   SERVER_CPUS=0,1,2 MT_CPUS=3-5 CONFIGS="vk-3t" scripts/bench_zipf.sh
#   VALKEY_BIN=…/valkey-src/src/valkey-server scripts/bench_zipf.sh
#
# - Un serveur à la fois, 1M clés préchargées (une fois chacune, pipeline 64).
# - memtier sur MT_CPUS (2–5), --distinct-client-seed --randomize.
# - Charge user / noyau / softirq de chaque CPU lue dans /proc/stat pendant la
#   mesure : le coût CPU par opération compte autant que le débit (le thread
#   d'E/S de Valkey tourne en attente active).
# - Un seul passage par cas : ordre de grandeur, pas de médiane.
# - Résultats dans $OUT/summary.txt.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_zipf.XXXX)}"
TEST_TIME="${TEST_TIME:-10}"
KEYS="${KEYS:-1000000}"
ZIPF="${ZIPF:-0.99}"
PIPELINES="${PIPELINES:-1 64}"
CONFIGS="${CONFIGS:-rx-1c rx-2c vk-1t vk-2t}"
# CPU du serveur, dans l'ordre où on les prend (défaut : cœurs physiques 0 et 1
# puis leurs jumeaux SMT 6 et 7, hors des CPU de memtier).
SERVER_CPUS="${SERVER_CPUS:-0,1,6,7}"
MT_CPUS="${MT_CPUS:-2-5}"
RX_BIN="${RX_BIN:-$ROOT/target/release/radixox}"
VALKEY_BIN="${VALKEY_BIN:-valkey-server}"
PORT="${PORT:-6440}"
MEM_MAX="${MEM_MAX:-4G}"

[ -x "$RX_BIN" ] || cargo build -q --release -p radixox --manifest-path "$ROOT/Cargo.toml"
command -v memtier_benchmark >/dev/null
SUMMARY="$OUT/summary.txt"
: > "$SUMMARY"

# cpu, user, noyau, irq+softirq, idle+iowait
snap() { awk '/^cpu[0-9]+ /{print $1, $2+$3, $4, $7+$8, $5+$6}' /proc/stat; }

mt() {
  taskset -c "$MT_CPUS" memtier_benchmark -s 127.0.0.1 -p "$PORT" --protocol=redis \
    -d 100 --key-prefix key: --key-minimum 1 --key-maximum "$KEYS" \
    --distinct-client-seed --randomize --hide-histogram \
    --print-percentiles 50,99,99.9 "$@" 2>/dev/null
}

start() {
  local cfg=$1 dir="$OUT/$1"
  rm -rf "$dir"; mkdir -p "$dir"
  local cpus; IFS=, read -ra cpus <<< "$SERVER_CPUS"
  [[ $cfg =~ ^(rx|vk)-([0-9]+)[ct]$ ]] || { echo "configuration inconnue : $cfg (rx-Nc ou vk-Nt)" >&2; exit 1; }
  local kind=${BASH_REMATCH[1]} n=${BASH_REMATCH[2]}
  (( n >= 1 && n <= ${#cpus[@]} )) || { echo "$cfg : N entre 1 et ${#cpus[@]} (SERVER_CPUS=$SERVER_CPUS)" >&2; exit 1; }
  local used; used=$(IFS=,; echo "${cpus[*]:0:n}")
  if [ "$kind" = rx ]; then
    local env=(RADIXOX_CPU_PIN="${cpus[0]}")
    if (( n > 1 )); then
      env+=(RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS=$((n - 1)) RADIXOX_IOWQ_CPUS="$(IFS=,; echo "${cpus[*]:1:n-1}")")
    fi
    env "${env[@]}" RADIXOX_PORT="$PORT" RADIXOX_SAVE="" RADIXOX_DIR="$dir" \
      systemd-run --user --scope -q -p MemoryMax="$MEM_MAX" "$RX_BIN" >"$dir/log" 2>&1 &
  else
    local bio=${cpus[$(( n < ${#cpus[@]} ? n : ${#cpus[@]} - 1 ))]}
    systemd-run --user --scope -q -p MemoryMax="$MEM_MAX" taskset -c "$used,$bio" "$VALKEY_BIN" \
      --port "$PORT" --bind 127.0.0.1 --save "" --appendonly no \
      --io-threads "$n" --server-cpulist "$used" --bio-cpulist "$bio" --dir "$dir" >"$dir/log" 2>&1 &
  fi
  for _ in $(seq 100); do redis-cli -p "$PORT" PING >/dev/null 2>&1 && break; sleep 0.1; done
  SPID=$(ss -ltnp | grep ":$PORT " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
  [ -n "$SPID" ] || { echo "$cfg : le serveur n'a pas démarré (voir $dir/log)" >&2; exit 1; }
}

stop() {
  kill "$SPID" 2>/dev/null || true
  while kill -0 "$SPID" 2>/dev/null; do sleep 0.1; done
}
trap stop EXIT

# CPU affichés : ceux du serveur, puis ceux de memtier (listes « 0,1,6 » ou « 2-5 »)
expand() { local a; for a in ${1//,/ }; do if [[ $a == *-* ]]; then seq "${a%-*}" "${a#*-}"; else echo "$a"; fi; done; }
SHOW=$( { expand "$SERVER_CPUS" | sed 's/^/serveur cpu/'; expand "$MT_CPUS" | sed 's/^/memtier cpu/'; } )

echo "Zipf $ZIPF, $KEYS clés, ${TEST_TIME} s par cas ; résultats : $OUT" | tee -a "$SUMMARY"
for cfg in $CONFIGS; do
  start "$cfg"
  mt --threads 1 --clients 1 --pipeline 64 --key-pattern P:P --ratio 1:0 --requests allkeys >/dev/null
  for p in $PIPELINES; do
    snap > "$OUT/s0"
    mt --threads 4 --clients 20 --pipeline "$p" --key-pattern Z:Z --key-zipf-exp "$ZIPF" \
       --ratio 1:4 --test-time "$TEST_TIME" > "$OUT/$cfg.p$p.txt"
    snap > "$OUT/s1"
    {
      awk -v c="$cfg" -v p="$p" '/^Totals/{printf "%-6s pipeline %-3s %10.0f ops/s   p50 %6s   p99 %6s   p99.9 %6s ms\n", c, p, $2, $(NF-3), $(NF-2), $(NF-1)}' "$OUT/$cfg.p$p.txt"
      join "$OUT/s0" "$OUT/s1" | awk -v show="$SHOW" 'BEGIN{n=split(show, l, "\n"); for (k=1; k<=n; k++) {split(l[k], f, " "); role[f[2]]=f[1]; ord[k]=f[2]}}
        {u=$6-$2; s=$7-$3; q=$8-$4; i=$9-$5; t=u+s+q+i; if (t>0) line[$1]=sprintf("         %-5s %-7s occupé %3.0f %%  user %3.0f  noyau %3.0f  softirq %3.0f", $1, role[$1], 100*(u+s+q)/t, 100*u/t, 100*s/t, 100*q/t)}
        END{for (k=1; k<=n; k++) if (ord[k] in line) print line[ord[k]]}'
    } | tee -a "$SUMMARY"
  done
  stop
done
