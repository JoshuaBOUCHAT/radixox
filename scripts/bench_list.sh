#!/bin/bash
# List : RadixOx contre Valkey (ANALYSE_ART_UNIFIE.md §14). Une passe, ~2 min.
#
#   scripts/bench_list.sh                 # mémoire + débit
#   SKIP_MEM=1 scripts/bench_list.sh      # débit seul
#   SKIP_TPUT=1 scripts/bench_list.sh     # mémoire seule
#   RX_BIN=… SKIP_VK=1 scripts/bench_list.sh   # autre binaire seul (A/B de taille de bloc)
#
# - Mémoire : un serveur neuf par cas ; RSS après chargement − RSS à vide, divisé par le
#   nombre d'éléments (clé comprise), 12 s après le chargement (état stable). Éléments de
#   16 o.
# - Débit : valkey-benchmark, pipeline 64, 50 connexions ; serveurs mono-thread (Valkey
#   `io-threads 1`), thread principal sur le CPU 0, client sur les CPU 2–5.
set -euo pipefail
export RADIXOX_SAVE="${RADIXOX_SAVE-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_list.XXXX)}"
N_REQ="${N_REQ:-2000000}"
RX_PORT=6513
VK_PORT=6514
CLIENT_CPUS=2-5
mkdir -p "$OUT"
command -v powerprofilesctl >/dev/null && powerprofilesctl set performance

if [ -z "${RX_BIN:-}" ]; then
  cargo build --release -q -p radixox --manifest-path "$ROOT/Cargo.toml"
  RX_BIN="$ROOT/target/release/radixox"
fi

pid=""
cleanup() { [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; }
trap cleanup EXIT

start() {  # rx|vk
  rm -f "$OUT/dump.rxd" "$OUT/dump.rdb"
  if [ "$1" = rx ]; then
    systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
      env RADIXOX_PORT=$RX_PORT RADIXOX_DIR="$OUT" RADIXOX_SAVE_ON_EXIT=0 taskset -c 0 "$RX_BIN" >"$OUT/rx.log" 2>&1 &
    PORT=$RX_PORT
  else
    systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
      taskset -c 0,1 valkey-server --port $VK_PORT --bind 127.0.0.1 --save "" --appendonly no \
      --io-threads 1 --server-cpulist 0 --bio-cpulist 1 --dir "$OUT" >"$OUT/vk.log" 2>&1 &
    PORT=$VK_PORT
  fi
  for _ in $(seq 50); do redis-cli -p $PORT PING >/dev/null 2>&1 && break; sleep 0.1; done
  pid=$(ss -ltnp | grep ":$PORT " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
}
stop() { kill "$pid"; while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done; pid=""; }
rss_kb() { awk '/VmRSS/ {print $2}' "/proc/$pid/status"; }

# Fichiers RESP (RPUSH par paquets de 100 éléments) : générés une fois.
gen() {  # nom
  [ -f "$OUT/$1.resp" ] && return
  python3 -I - "$1" >"$OUT/$1.resp" <<'EOF'
import sys
case = sys.argv[1]
w = sys.stdout.buffer.write
def rpush(key, vals):
    args = [b"RPUSH", key] + vals
    w(b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args))
def el(i):
    return b"element:%08d" % i  # 16 o
if case == "big":        # 1 List, 1 M éléments
    for b in range(0, 1000000, 100):
        rpush(b"biglist", [el(j) for j in range(b, b + 100)])
elif case == "small":    # 100 000 List × 10 éléments
    for k in range(100000):
        rpush(b"l:%06d" % k, [el(j) for j in range(10)])
elif case == "queues":   # 10 000 List × 100 éléments (`l:%012d`, ce que produit `__rand_int__`)
    for k in range(10000):
        rpush(b"l:%012d" % k, [el(j) for j in range(100)])
EOF
}

load() { redis-cli -p $PORT --pipe <"$OUT/$1.resp" | grep -q "errors: 0" || { echo "chargement $1 en erreur" >&2; exit 1; }; }

{
echo "# List RadixOx ($(git -C "$ROOT" rev-parse --short HEAD)) contre $(valkey-server --version | cut -d' ' -f3)"
if [ -z "${SKIP_MEM:-}" ]; then
  echo
  echo "## Mémoire (RSS par élément, clé comprise, éléments de 16 o, +12 s)"
  printf "%-28s %-12s %-12s\n" cas RadixOx Valkey
  for c in big small; do
    gen $c
    line=""
    for s in rx $([ -z "${SKIP_VK:-}" ] && echo vk); do
      start $s
      sleep 0.3
      before=$(rss_kb)
      load $c
      sleep 12
      after=$(rss_kb)
      line="$line $(printf "%-12s" "$(( (after - before) * 1024 / 1000000 )) o")"
      stop
    done
    case $c in
      big) name="1 × 1 M" ;;
      small) name="100 000 × 10" ;;
    esac
    printf "%-28s %s\n" "$name" "$line"
  done
fi
if [ -z "${SKIP_TPUT:-}" ]; then
  gen queues
  gen big
  echo
  echo "## Débit (pipeline 64, 50 connexions, $N_REQ requêtes)"
  for s in rx $([ -z "${SKIP_VK:-}" ] && echo vk); do
    start $s
    load queues
    load big
    for t in "LPUSH l:__rand_int__ element:00000000" \
             "RPOP l:__rand_int__" \
             "RPUSH l:__rand_int__ element:00000000" \
             "LPOP l:__rand_int__" \
             "LRANGE l:__rand_int__ 0 9" \
             "LRANGE biglist 0 99" \
             "LINDEX biglist 500000" \
             "LLEN l:__rand_int__"; do
      r=$(taskset -c $CLIENT_CPUS valkey-benchmark -p $PORT -c 50 -P 64 --threads 2 -n "$N_REQ" -r 10000 -q $t \
        | tr '\r' '\n' | grep "requests per second" | tail -1)
      printf "%-3s %-42s %s\n" "$s" "$t" "$r"
    done
    echo "    RSS $(( $(rss_kb) / 1024 )) Mo"
    stop
  done
fi
} | tee "$OUT/summary.txt"
echo "résultats : $OUT/summary.txt"
