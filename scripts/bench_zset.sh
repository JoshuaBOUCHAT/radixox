#!/bin/bash
# ZSet : RadixOx contre Valkey (ANALYSE_ART_UNIFIE.md §13.9). Une passe.
#
#   scripts/bench_zset.sh                 # ~3 min
#   SKIP_MEM=1 scripts/bench_zset.sh      # débit seul
#   SKIP_TPUT=1 scripts/bench_zset.sh     # mémoire seule
#
# - Mémoire : un serveur neuf par cas ; RSS après chargement − RSS à vide, divisé par le
#   nombre de membres (clé comprise), 1 s puis 12 s après le chargement (état stable :
#   GC du pool passé, pages vides rendues après RADIXOX_MEMORY_DECAY = 10 s ; decay de
#   jemalloc pour Valkey). Membres de 15 o (`member:%07d`).
# - Débit : valkey-benchmark, pipeline 64, 50 connexions, sur un ZSet d'1 M membres
#   (`m:%012d`, ce que produit `__rand_int__`). Serveurs mono-thread (Valkey
#   `io-threads 1`), thread principal sur le CPU 0 (bio de Valkey sur le CPU 1), client sur
#   les CPU 2–5. Profil d'énergie `performance`.
set -euo pipefail
export RADIXOX_SAVE="${RADIXOX_SAVE-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_zset.XXXX)}"
N_REQ="${N_REQ:-2000000}"
RX_PORT=6511
VK_PORT=6512
CLIENT_CPUS=2-5
mkdir -p "$OUT"
command -v powerprofilesctl >/dev/null && powerprofilesctl set performance

cargo build --release -q -p radixox --manifest-path "$ROOT/Cargo.toml"
RX_BIN="$ROOT/target/release/radixox"

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

# Fichiers RESP (ZADD par paquets de 100 membres) : générés une fois.
gen() {  # nom
  python3 -I - "$1" >"$OUT/$1.resp" <<'EOF'
import sys
case = sys.argv[1]
w = sys.stdout.buffer.write
def zadd(key, pairs):
    args = [b"ZADD", key]
    for s, m in pairs:
        args += [s, m]
    w(b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args))
def mem(i):
    return b"member:%07d" % i  # 15 o
if case == "large":      # 1000 ZSet × 1000 membres, scores entiers
    for k in range(1000):
        for b in range(0, 1000, 100):
            zadd(b"zset:%04d" % k, [(b"%d" % (j * 7 % 1000), mem(j)) for j in range(b, b + 100)])
elif case == "float":    # idem, scores flottants
    for k in range(1000):
        for b in range(0, 1000, 100):
            zadd(b"zset:%04d" % k, [(b"%.3f" % (j * 1.37), mem(j)) for j in range(b, b + 100)])
elif case == "small":    # 100 000 ZSet × 10 membres
    for k in range(100000):
        zadd(b"zs:%06d" % k, [(b"%d" % j, mem(j)) for j in range(10)])
elif case == "seq":      # 1 ZSet, 1 M membres, scores croissants (horodatages en ms)
    for b in range(0, 1000000, 100):
        zadd(b"zseq", [(b"%d" % (1700000000000 + j), mem(j)) for j in range(b, b + 100)])
elif case == "ties":     # 1 ZSet, 1 M membres, tous à 0
    for b in range(0, 1000000, 100):
        zadd(b"zties", [(b"0", mem(j)) for j in range(b, b + 100)])
elif case == "tput":     # 1 ZSet, 1 M membres `m:%012d`, scores aléatoires
    import random
    r = random.Random(1)
    for b in range(0, 1000000, 100):
        zadd(b"bigz", [(b"%d" % r.randrange(1000000), b"m:%012d" % j) for j in range(b, b + 100)])
EOF
}

load() { redis-cli -p $PORT --pipe <"$OUT/$1.resp" | grep -q "errors: 0" || { echo "chargement $1 en erreur" >&2; exit 1; }; }

{
echo "# ZSet RadixOx ($(git -C "$ROOT" rev-parse --short HEAD)) contre $(valkey-server --version | cut -d' ' -f3)"
if [ -z "${SKIP_MEM:-}" ]; then
  echo
  echo "## Mémoire (RSS par membre, clé comprise, 1 M membres de 15 o)"
  printf "%-34s %-26s %-26s\n" cas "RadixOx (+1 s, +12 s)" "Valkey (+1 s, +12 s)"
  for c in large small float seq ties; do
    gen $c
    line=""
    for s in rx vk; do
      start $s
      sleep 0.3
      before=$(rss_kb)
      t0=$(date +%s%N)
      load $c
      t1=$(date +%s%N)
      sleep 1
      a1=$(rss_kb)
      sleep 11
      a12=$(rss_kb)
      p1=$(( (a1 - before) * 1024 / 1000000 ))
      p12=$(( (a12 - before) * 1024 / 1000000 ))
      line="$line $(printf "%4s o %4s o  %4s ms     " "$p1" "$p12" "$(( (t1 - t0) / 1000000 ))")"
      stop
    done
    case $c in
      large) name="grands (1000 × 1000), entiers" ;;
      small) name="petits (100 000 × 10)" ;;
      float) name="grands (1000 × 1000), flottants" ;;
      seq) name="1 × 1 M, scores croissants" ;;
      ties) name="1 × 1 M, tous à 0" ;;
    esac
    printf "%-34s %s\n" "$name" "$line"
  done
  echo "(o/membre à +1 s et à +12 s, puis durée du chargement par redis-cli --pipe)"
fi
if [ -z "${SKIP_TPUT:-}" ]; then
  gen tput
  echo
  echo "## Débit (pipeline 64, 50 connexions, $N_REQ requêtes, 1 ZSet d'1 M membres)"
  for s in rx vk; do
    start $s
    load tput
    for t in "ZSCORE bigz m:__rand_int__" \
             "ZADD bigz __rand_int__ m:__rand_int__" \
             "ZINCRBY bigz 1 m:__rand_int__" \
             "ZRANK bigz m:__rand_int__" \
             "ZRANGE bigz 500000 500009 WITHSCORES" \
             "ZRANGEBYSCORE bigz __rand_int__ +inf LIMIT 0 10"; do
      r=$(taskset -c $CLIENT_CPUS valkey-benchmark -p $PORT -c 50 -P 64 --threads 2 -n "$N_REQ" -r 1000000 -q $t \
        | tr '\r' '\n' | grep "requests per second" | tail -1)
      printf "%-3s %-48s %s\n" "$s" "$t" "$r"
    done
    # petits ZSet : ZADD dispersé sur 100 000 clés
    r=$(taskset -c $CLIENT_CPUS valkey-benchmark -p $PORT -c 50 -P 64 --threads 2 -n "$N_REQ" -r 100000 -q \
      ZADD z:__rand_int__ __rand_int__ m:__rand_int__ | tr '\r' '\n' | grep "requests per second" | tail -1)
    printf "%-3s %-48s %s\n" "$s" "ZADD z:__rand_int__ (petits ZSet)" "$r"
    echo "    RSS $(( $(rss_kb) / 1024 )) Mo"
    stop
  done
fi
} | tee "$OUT/summary.txt"
echo "résultats : $OUT/summary.txt"
