#!/bin/bash
# ZSet : opérations ensemblistes et ZSCAN, RadixOx contre Valkey (ANALYSE_ART_UNIFIE.md §13.12).
# Une passe, ~1 min 30.
#
#   scripts/bench_zset_ops.sh
#   N_SMALL=300000 N_MED=3000 scripts/bench_zset_ops.sh    # requêtes par test
#
# - Débit : valkey-benchmark, pipeline 16, 50 connexions. Paires de ZSet de même forme,
#   moitié des membres en commun ; `__rand_int__` tire une paire au hasard à chaque
#   requête (chaque occurrence indépendamment) :
#     petits  : 1 000 paires `sa:` / `sb:` × 100 membres (forme compacte des deux côtés) ;
#     moyens  : 100 paires `ma:` / `mb:` × 10 000 membres ;
#     un Set  : `ss:` (Set de 100 membres) en entrée de ZINTER.
#   STORE : destination tirée parmi 1 000 clés (`d:`), écrasée à chaque fois.
# - ZSCAN : un client, parcours complet d'un ZSet d'1 M membres (COUNT 100 puis 1 000) ;
#   durée et nombre d'appels (Valkey : ordre du dict, RadixOx : ordre du ZSet).
# - Serveurs mono-thread (Valkey `io-threads 1`), thread principal sur le CPU 0 (bio de
#   Valkey sur le CPU 1), client sur les CPU 2–5. Profil d'énergie `performance`.
set -euo pipefail
export RADIXOX_SAVE="${RADIXOX_SAVE-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$(mktemp -d /tmp/bench_zset_ops.XXXX)}"
N_SMALL="${N_SMALL:-100000}"
N_MED="${N_MED:-1000}"
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

# Jeu de données (RESP, généré une fois) : clés en `%012d` comme `__rand_int__`.
[ -s "$OUT/data.resp" ] || python3 -I - >"$OUT/data.resp" <<'EOF'
import sys
w = sys.stdout.buffer.write
def cmd(*args):
    w(b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args))
def zadd(key, members):
    for b in range(0, len(members), 500):
        args = [b"ZADD", key]
        for s, m in members[b:b + 500]:
            args += [b"%d" % s, m]
        cmd(*args)
def pair(a, b, i, n):
    # moitié commune : a = [0, n), b = [n/2, 3n/2)
    zadd(a + b"%012d" % i, [(j * 7 % 1000, b"member:%07d" % j) for j in range(n)])
    zadd(b + b"%012d" % i, [(j * 3 % 1000, b"member:%07d" % j) for j in range(n // 2, n + n // 2)])
for i in range(1000):
    pair(b"sa:", b"sb:", i, 100)
    cmd(b"SADD", b"ss:%012d" % i, *[b"member:%07d" % j for j in range(0, 200, 2)])
for i in range(100):
    pair(b"ma:", b"mb:", i, 10000)
zadd(b"bigz", [(j % 100000, b"m:%012d" % j) for j in range(1000000)])
EOF

load() { redis-cli -p $PORT --pipe <"$OUT/data.resp" | grep -q "errors: 0" || { echo "chargement en erreur" >&2; exit 1; }; }

bench() {  # n r commande...
  local n=$1 r=$2; shift 2
  taskset -c $CLIENT_CPUS valkey-benchmark -p $PORT -c 50 -P 16 --threads 2 -n "$n" -r "$r" -q "$@" \
    | tr '\r' '\n' | grep "requests per second" | tail -1 | sed -E 's/.*: ([0-9.]+) requests per second.*/\1/'
}

zscan_full() {  # count → "durée_ms appels membres"
  python3 -I - "$PORT" "$1" <<'EOF'
import socket, sys, time
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
f = s.makefile("rb")
def read():
    line = f.readline()
    t, rest = line[:1], line[1:-2]
    if t == b"$":
        return f.read(int(rest) + 2)[:-2]
    if t == b"*":
        return [read() for _ in range(int(rest))]
    if t == b"-":
        raise RuntimeError(rest)
    return rest
cur, calls, seen = b"0", 0, set()
t = time.perf_counter()
while True:
    q = [b"ZSCAN", b"bigz", cur, b"COUNT", sys.argv[2].encode()]
    s.sendall(b"*%d\r\n" % len(q) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in q))
    cur, items = read()
    seen.update(items[0::2])
    calls += 1
    if cur == b"0":
        break
print(f"{(time.perf_counter() - t) * 1000:.0f} {calls} {len(seen)}")
EOF
}

TESTS=(
  "S|ZUNION 2 sa:__rand_int__ sb:__rand_int__"
  "S|ZUNION 2 sa:__rand_int__ sb:__rand_int__ WITHSCORES"
  "S|ZINTER 2 sa:__rand_int__ sb:__rand_int__ WITHSCORES"
  "S|ZDIFF 2 sa:__rand_int__ sb:__rand_int__"
  "S|ZINTER 2 sa:__rand_int__ ss:__rand_int__ AGGREGATE MAX"
  "S|ZUNIONSTORE d:__rand_int__ 2 sa:__rand_int__ sb:__rand_int__ WEIGHTS 2 1"
  "S|ZINTERSTORE d:__rand_int__ 2 sa:__rand_int__ sb:__rand_int__"
  "S|ZINTERCARD 2 sa:__rand_int__ sb:__rand_int__"
  "S|ZINTERCARD 2 sa:__rand_int__ sb:__rand_int__ LIMIT 10"
  "M|ZUNION 2 ma:__rand_int__ mb:__rand_int__ WITHSCORES"
  "M|ZINTER 2 ma:__rand_int__ mb:__rand_int__ WITHSCORES"
  "M|ZDIFF 2 ma:__rand_int__ mb:__rand_int__"
  "M|ZINTERSTORE d:__rand_int__ 2 ma:__rand_int__ mb:__rand_int__"
  "M|ZINTERCARD 2 ma:__rand_int__ mb:__rand_int__"
)

declare -A RES
for s in rx vk; do
  start $s
  load
  for t in "${TESTS[@]}"; do
    kind=${t%%|*}; c=${t#*|}
    if [ "$kind" = S ]; then RES[$s|$c]=$(bench "$N_SMALL" 1000 $c); else RES[$s|$c]=$(bench "$N_MED" 100 $c); fi
  done
  for n in 100 1000; do RES[$s|zscan$n]=$(zscan_full $n); done
  stop
done

{
echo "# ZSet, opérations ensemblistes : RadixOx ($(git -C "$ROOT" rev-parse --short HEAD)) contre $(valkey-server --version | cut -d' ' -f3)"
echo
echo "## Débit (req/s, pipeline 16, 50 connexions)"
printf "%-72s %12s %12s %7s\n" commande RadixOx Valkey ratio
for t in "${TESTS[@]}"; do
  kind=${t%%|*}; c=${t#*|}
  [ "$kind" = S ] && tag="[100]" || tag="[10k]"
  a=${RES[rx|$c]}; b=${RES[vk|$c]}
  printf "%-72s %12s %12s %6sx\n" "$tag $c" "$a" "$b" "$(awk -v a="$a" -v b="$b" 'BEGIN { printf "%.2f", (b > 0 ? a / b : 0) }')"
done
echo "([100] : paires de 100 membres, $N_SMALL requêtes ; [10k] : paires de 10 000 membres, $N_MED requêtes)"
echo "(vers $((N_SMALL * 2 / 1000)) k req/s : plafond de l'outil à $N_SMALL requêtes (~0,5 s fixes) ; monter N_SMALL pour départager)"
echo
echo "## ZSCAN complet d'1 M membres (un client, sans pipeline)"
printf "%-12s %-28s %-28s\n" COUNT "RadixOx (ms, appels, vus)" "Valkey (ms, appels, vus)"
for n in 100 1000; do printf "%-12s %-28s %-28s\n" $n "${RES[rx|zscan$n]}" "${RES[vk|zscan$n]}"; done
} | tee "$OUT/summary.txt"
echo "résultats : $OUT/summary.txt"
