#!/bin/bash
# RadixOx contre Valkey en accès réaliste : clés tirées en Zipf (0,99 par défaut,
# quelques clés chaudes, une longue traîne), 1:4 SET:GET, 100 o. Chaque
# configuration serveur passe sans pipeline puis en pipeline 64 :
#
#   rx-Nc   RadixOx : boucle sur le 1er CPU de SERVER_CPUS, N−1 workers io-wq
#           (envois async) sur les suivants ; rx-1c = boucle seule
#   vk-Nt   Valkey io-threads N sur les N premiers CPU de SERVER_CPUS, ses
#           threads bio (au repos dans ce bench) sur les mêmes CPU
#
#   scripts/bench_zipf.sh                         # ~2 min 30
#   TEST_TIME=5 PIPELINES=64 scripts/bench_zipf.sh   # essai rapide
#   CONFIGS="rx-1c vk-1t" ZIPF=1.2 scripts/bench_zipf.sh
#   SERVER_CORES=3 CONFIGS="rx-2c rx-3c" scripts/bench_zipf.sh   # même client que vk-3t
#   CONFIGS="rx-3c vk-3t" scripts/bench_zipf.sh    # 3 cœurs physiques
#   SERVER_CPUS=0,1,6 MT_CPUS=2-5 CONFIGS="vk-3t" scripts/bench_zipf.sh   # à la main
#   VALKEY_BIN=…/valkey-src/src/valkey-server scripts/bench_zipf.sh
#
# - Un serveur à la fois, 1M clés préchargées (une fois chacune, pipeline 64).
# - Placement automatique (topologie lue dans sysfs), sauf SERVER_CPUS / MT_CPUS
#   donnés : le serveur prend un thread par cœur physique, les cœurs
#   performants d'abord (cpu_capacity la plus haute ; sur un hybride Intel, les
#   cœurs efficaces sont aussi listés dans /sys/devices/cpu_atom/cpus) ; autant
#   de cœurs que SERVER_CORES (défaut : le plus grand N de CONFIGS). Les
#   placements, donc le CPU du client, ne dépendent que de ce nombre : pour
#   comparer deux lancements, garder le même SERVER_CORES. Les jumeaux SMT de ces cœurs restent libres ; memtier prend tous les
#   CPU des autres cœurs.
# - memtier : un thread par CPU de MT_CPUS (MT_THREADS), MT_CONNS (80)
#   connexions en tout, --distinct-client-seed --randomize. Si ses CPU passent
#   85 % d'occupation, le cas est signalé : le débit mesure alors le client.
# - Charge user / noyau / softirq de chaque CPU lue dans /proc/stat pendant la
#   mesure : le coût CPU par opération compte autant que le débit (le thread
#   d'E/S de Valkey tourne en attente active).
# - RSS du serveur (/proc/PID/status) : après le préchargement, après chaque
#   mesure, et pic depuis le démarrage (VmHWM).
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
MT_CONNS="${MT_CONNS:-80}"
RX_BIN="${RX_BIN:-$ROOT/target/release/radixox}"
VALKEY_BIN="${VALKEY_BIN:-valkey-server}"
PORT="${PORT:-6440}"
MEM_MAX="${MEM_MAX:-4G}"

[ -x "$RX_BIN" ] || cargo build -q --release -p radixox --manifest-path "$ROOT/Cargo.toml"
command -v memtier_benchmark >/dev/null
SUMMARY="$OUT/summary.txt"
: > "$SUMMARY"

# Cœurs physiques en ligne, un par ligne : « classe premier_cpu cpus » (classe
# P = performant, E = efficace), P d'abord puis par numéro de CPU.
cores() {
  local atom="" d c cap
  [ -r /sys/devices/cpu_atom/cpus ] && atom=$(cat /sys/devices/cpu_atom/cpus)
  for d in /sys/devices/system/cpu/cpu[0-9]*; do
    c=${d##*cpu}
    [ "$(cat "$d/online" 2>/dev/null || echo 1)" = 1 ] || continue
    cap=$(cat "$d/cpu_capacity" 2>/dev/null || echo 1024)
    if [ -n "$atom" ] && expand "$atom" | grep -qx "$c"; then cap=0; fi
    echo "$c $cap $(cat "$d/topology/core_cpus_list" 2>/dev/null || echo "$c")"
  done | awk '{if (!($3 in first) || $1 < first[$3]) first[$3]=$1; if ($2 > cap[$3]) cap[$3]=$2; if ($2 > max) max=$2}
    END{for (k in first) printf "%s %d %s\n", (cap[k] >= 0.9*max ? "P" : "E"), first[k], k}' |
    sort -k1,1r -k2,2n
}

# Liste « 0,1,6 » ou « 2-5 » → un CPU par ligne
expand() { local a; for a in ${1//,/ }; do if [[ $a == *-* ]]; then seq "${a%-*}" "${a#*-}"; else echo "$a"; fi; done; }

if [ -z "${SERVER_CPUS:-}" ] || [ -z "${MT_CPUS:-}" ]; then
  need=${SERVER_CORES:-0}
  if [ -z "${SERVER_CORES:-}" ]; then
    for cfg in $CONFIGS; do n=${cfg//[!0-9]/}; (( ${n:-0} > need )) && need=$n; done
  fi
  mapfile -t CORES < <(cores)
  (( need < ${#CORES[@]} )) || { echo "il faut $need cœurs physiques pour le serveur et au moins un pour memtier ; la machine en a ${#CORES[@]}" >&2; exit 1; }
  srv=(); mt=()
  for i in "${!CORES[@]}"; do
    read -r cls first list <<< "${CORES[$i]}"
    if (( i < need )); then srv+=("$first"); else mt+=($(expand "$list")); fi
  done
  SERVER_CPUS="${SERVER_CPUS:-$(IFS=,; echo "${srv[*]}")}"
  MT_CPUS="${MT_CPUS:-$(IFS=,; echo "${mt[*]}")}"
  echo "topologie : $(printf '%s\n' "${CORES[@]}" | awk '{n[$1]++; l=l" "$3} END{printf "%d cœurs P, %d cœurs E ; CPU par cœur physique :%s\n", n["P"], n["E"], l}')"
  echo "placement : serveur sur $SERVER_CPUS (jumeaux SMT libres), memtier sur $MT_CPUS"
fi

MT_THREADS="${MT_THREADS:-$(expand "$MT_CPUS" | wc -l)}"
MT_CLIENTS=$(( (MT_CONNS + MT_THREADS - 1) / MT_THREADS ))
echo "memtier : $MT_THREADS threads × $MT_CLIENTS clients"

# RSS du serveur en Mo : « VmRSS VmHWM »
rss() { awk '/^VmRSS/{r=$2} /^VmHWM/{h=$2} END{printf "%.0f %.0f", r/1024, h/1024}' "/proc/$SPID/status"; }

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
    systemd-run --user --scope -q -p MemoryMax="$MEM_MAX" taskset -c "$used" "$VALKEY_BIN" \
      --port "$PORT" --bind 127.0.0.1 --save "" --appendonly no \
      --io-threads "$n" --server-cpulist "$used" --bio-cpulist "$used" --dir "$dir" >"$dir/log" 2>&1 &
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

# CPU affichés : ceux du serveur, puis ceux de memtier
# puis tous les autres (« libre » : jumeaux SMT du serveur, CPU inutilisés)
SHOW=$( { expand "$SERVER_CPUS" | sed 's/^/serveur cpu/'; expand "$MT_CPUS" | sed 's/^/memtier cpu/'
          ls -d /sys/devices/system/cpu/cpu[0-9]* | sed 's|.*/cpu||' | sort -n | sed 's/^/libre cpu/'; } |
        awk '!seen[$2]++' )

echo "Zipf $ZIPF, $KEYS clés, ${TEST_TIME} s par cas ; résultats : $OUT" | tee -a "$SUMMARY"
for cfg in $CONFIGS; do
  start "$cfg"
  mt --threads 1 --clients 1 --pipeline 64 --key-pattern P:P --ratio 1:0 --requests allkeys >/dev/null
  read -r RSS_LOAD _ <<< "$(rss)"
  for p in $PIPELINES; do
    snap > "$OUT/s0"
    mt --threads "$MT_THREADS" --clients "$MT_CLIENTS" --pipeline "$p" --key-pattern Z:Z --key-zipf-exp "$ZIPF" \
       --ratio 1:4 --test-time "$TEST_TIME" > "$OUT/$cfg.p$p.txt"
    snap > "$OUT/s1"
    read -r rss_now rss_peak <<< "$(rss)"
    {
      awk -v c="$cfg" -v p="$p" '/^Totals/{printf "%-6s pipeline %-3s %10.0f ops/s   moy %6.3f   p50 %6.3f   p99 %6.3f   p99.9 %6.3f ms\n", c, p, $2, $(NF-4), $(NF-3), $(NF-2), $(NF-1)}' "$OUT/$cfg.p$p.txt"
      join "$OUT/s0" "$OUT/s1" | awk -v show="$SHOW" -v rl="$RSS_LOAD" -v rn="$rss_now" -v rp="$rss_peak" 'BEGIN{n=split(show, l, "\n"); for (k=1; k<=n; k++) {split(l[k], f, " "); role[f[2]]=f[1]; ord[k]=f[2]}}
        {u=$6-$2; s=$7-$3; q=$8-$4; i=$9-$5; t=u+s+q+i; if (t>0) line[$1]=sprintf("         %-5s %-7s occupé %3.0f %%  user %3.0f  noyau %3.0f  softirq %3.0f", $1, role[$1], 100*(u+s+q)/t, 100*u/t, 100*s/t, 100*q/t)}
        {if (role[$1]=="memtier" && t>0) {mt+=100*(u+s+q)/t; nm++}}
        END{for (k=1; k<=n; k++) if (ord[k] in line) print line[ord[k]]
            printf "         RSS  chargé %s Mo  après %s Mo  pic %s Mo\n", rl, rn, rp
            if (nm && mt/nm > 85) printf "         ⚠ memtier saturé (%.0f %% en moyenne) : ce débit est celui du client\n", mt/nm}'
    } | tee -a "$SUMMARY"
  done
  stop
done
