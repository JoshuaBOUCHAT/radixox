#!/bin/bash
set -euo pipefail
# Pas de save automatique pendant un bench (Valkey y tourne avec --save "").
export RADIXOX_SAVE="${RADIXOX_SAVE-}"
export LC_ALL=C   # printf %f / awk : point décimal quelle que soit la locale

# YCSB workload A (même config que bench_compare.sh) sur RadixOx uniquement,
# pour comparer plusieurs versions de RadixOx entre elles.
#
# Usage:
#   ./bench_radixox.sh                       # working tree seul
#   ./bench_radixox.sh HEAD WORKTREE         # HEAD vs working tree (non commité)
#   ./bench_radixox.sh main custom_parser    # n'importe quels refs git
#   ./bench_radixox.sh WORKTREE VALKEY       # VALKEY = valkey-server (pas de build),
#                                            # VALKEY_IO_THREADS=2 → main + 1 io-thread
#
# Chaque ref est buildée dans un worktree git temporaire (WORKTREE = arbre
# courant, modifs non commitées incluses). Les versions sont ensuite benchées
# en alternance (A B A B ...) sur ROUNDS tours pour lisser le bruit thermique /
# système ; le résumé donne la médiane par version.
#
# Variables d'env : RECORDS OPS THREADS FIELDLENGTH ROUNDS PORT
#                   SERVER_CPUS (défaut 0-1)  CLIENT_CPUS (défaut 2-5)
#                   OUT_DIR (défaut /tmp/bench_radixox)
#                   FULL_KEYSPACE=1 : le run tape les RECORDS clés (bench_compare.sh,
#                   lui, ne passe pas recordcount en run → seulement 1000 clés)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
YCSB_DIR="$(cd "$SCRIPT_DIR/../ycsb-redis-binding-0.18.0-SNAPSHOT" && pwd)"
WORKLOAD="workloads/workloada"
PORT="${PORT:-6379}"
RECORDS="${RECORDS:-5000000}"
OPS="${OPS:-10000000}"
THREADS="${THREADS:-100}"
FIELDLENGTH="${FIELDLENGTH:-100}"
ROUNDS="${ROUNDS:-3}"
SERVER_CPUS="${SERVER_CPUS:-0-1}"
CLIENT_CPUS="${CLIENT_CPUS:-2-5}"
OUT_DIR="${OUT_DIR:-/tmp/bench_radixox}"
FULL_KEYSPACE="${FULL_KEYSPACE:-0}"

REFS=("$@")
[ ${#REFS[@]} -eq 0 ] && REFS=(WORKTREE)

mkdir -p "$OUT_DIR/bin"
CURRENT_LOG=""

cleanup() {
  local exit_code=$?
  if [ $exit_code -ne 0 ] && [ -n "$CURRENT_LOG" ] && [ -f "$CURRENT_LOG" ]; then
    echo ""
    echo "ERROR: exit $exit_code — dernières lignes de $CURRENT_LOG :"
    tail -30 "$CURRENT_LOG"
  fi
  kill_port 2>/dev/null || true
}
trap cleanup EXIT

kill_port() {
  local pid
  pid=$(ss -tlnp "sport = :$PORT" 2>/dev/null | grep -oP '(?<=pid=)\d+' | head -1 || true)
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      ss -tlnp "sport = :$PORT" 2>/dev/null | grep -q ":$PORT" || return 0
      sleep 0.3
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

wait_for_server() {
  for _ in $(seq 1 60); do
    redis-cli -p "$PORT" PING 2>/dev/null | grep -q PONG && return 0
    sleep 0.5
  done
  echo "TIMEOUT en attendant le serveur" >&2
  return 1
}

peak_rss_mb() {
  awk '/VmHWM/ {printf "%.0f", $2/1024}' "/proc/$1/status" 2>/dev/null || echo "N/A"
}

run_ycsb() {
  local phase=$1 outfile=$2 count_arg
  if [ "$phase" = "load" ]; then
    count_arg="-p recordcount=$RECORDS"
  elif [ "$FULL_KEYSPACE" = "1" ]; then
    count_arg="-p recordcount=$RECORDS -p operationcount=$OPS"
  else
    # Comme bench_compare.sh : recordcount non passé en run → YCSB reprend
    # recordcount=1000 du workload, le run ne tape que user0..user999.
    count_arg="-p operationcount=$OPS"
  fi
  taskset -c "$CLIENT_CPUS" "$YCSB_DIR/bin/ycsb.sh" "$phase" redis -s -P "$YCSB_DIR/$WORKLOAD" \
    -p redis.host=127.0.0.1 -p redis.port="$PORT" \
    -p fieldcount=1 -p fieldnamekey=false \
    -p fieldlength="$FIELDLENGTH" \
    $count_arg \
    -threads "$THREADS" \
    -p percentiles=95,99,99.9,99.99 \
    >"$outfile" 2>&1
}

parse_stat() {
  grep -F "$2" "$1" 2>/dev/null | awk '{print $3}' | head -1 || true
}

parse_inline_pct() {
  grep -oP "\[$2:[^\]]*\]" "$1" 2>/dev/null | tail -1 | grep -oP "${3}=\K[0-9]+" || true
}

sanitize() { echo "$1" | tr '/:~^' '____'; }

# ============================================
# Build de chaque version
# ============================================
build_ref() {
  local ref=$1 name src target
  name=$(sanitize "$ref")
  if [ "$ref" = "WORKTREE" ]; then
    src="$SCRIPT_DIR"
    target="$SCRIPT_DIR/target"
  else
    src="$OUT_DIR/wt/$name"
    target="$OUT_DIR/target/$name"
    rm -rf "$src"
    git -C "$SCRIPT_DIR" worktree prune
    git -C "$SCRIPT_DIR" worktree add --detach --force "$src" "$ref" >/dev/null
  fi
  echo "  [$ref] build ($(git -C "$src" rev-parse --short HEAD)$( [ "$ref" = WORKTREE ] && echo ' + modifs non commitées'))..."
  RUSTFLAGS="-C target-cpu=native" CARGO_TARGET_DIR="$target" \
    cargo build --release --bin radixox --manifest-path "$src/Cargo.toml" 2>&1 | tail -1
  cp "$target/release/radixox" "$OUT_DIR/bin/$name"
  if [ "$ref" != "WORKTREE" ]; then
    git -C "$SCRIPT_DIR" worktree remove --force "$src"
  fi
}

echo "═══════════════════════════════════════════════════"
echo "  YCSB workload A — RadixOx seul"
echo "  Versions: ${REFS[*]}"
echo "  Records: $RECORDS | Ops: $OPS | Threads: $THREADS | Rounds: $ROUNDS"
echo "  Run keyspace: $([ "$FULL_KEYSPACE" = 1 ] && echo "$RECORDS clés" || echo "1000 clés (comme bench_compare)")"
echo "  Field length: $FIELDLENGTH | server cpus $SERVER_CPUS | client cpus $CLIENT_CPUS"
echo "═══════════════════════════════════════════════════"
echo "[build]"
for ref in "${REFS[@]}"; do [ "$ref" = VALKEY ] || build_ref "$ref"; done
echo ""

# ============================================
# Bench (alterné)
# ============================================
RESULTS="$OUT_DIR/results.tsv"
printf "ref\tround\tload_ops\tload_p99\trun_ops\tread_avg\tread_p99\tread_p999\tread_p9999\tupd_avg\tupd_p99\trss_mb\n" >"$RESULTS"

for round in $(seq 1 "$ROUNDS"); do
  for ref in "${REFS[@]}"; do
    name=$(sanitize "$ref")
    load_log="$OUT_DIR/${name}_r${round}_load.txt"
    run_log="$OUT_DIR/${name}_r${round}_run.txt"

    kill_port || true
    if [ "$ref" = VALKEY ]; then
      taskset -c "$SERVER_CPUS" valkey-server --port "$PORT" --daemonize no --save "" \
        --appendonly no --dir /tmp --io-threads "${VALKEY_IO_THREADS:-1}" \
        >"$OUT_DIR/${name}_r${round}_server.log" 2>&1 &
    else
      taskset -c "$SERVER_CPUS" "$OUT_DIR/bin/$name" >"$OUT_DIR/${name}_r${round}_server.log" 2>&1 &
    fi
    pid=$!
    wait_for_server

    CURRENT_LOG=$load_log
    run_ycsb load "$load_log"
    CURRENT_LOG=$run_log
    run_ycsb run "$run_log"
    rss=$(peak_rss_mb "$pid")
    kill_port

    row=(
      "$ref" "$round"
      "$(parse_stat "$load_log" "Throughput(ops/sec)")"
      "$(parse_stat "$load_log" "[INSERT], 99thPercentileLatency")"
      "$(parse_stat "$run_log" "Throughput(ops/sec)")"
      "$(parse_stat "$run_log" "[READ], AverageLatency")"
      "$(parse_stat "$run_log" "[READ], 99thPercentileLatency")"
      "$(parse_inline_pct "$run_log" READ 99.9)"
      "$(parse_inline_pct "$run_log" READ 99.99)"
      "$(parse_stat "$run_log" "[UPDATE], AverageLatency")"
      "$(parse_stat "$run_log" "[UPDATE], 99thPercentileLatency")"
      "$rss"
    )
    (IFS=$'\t'; echo "${row[*]}") >>"$RESULTS"
    printf "  round %s %-16s load %10.0f ops/s | run %10.0f ops/s | READ p99 %s µs | RSS %s MB\n" \
      "$round" "$ref" "${row[2]}" "${row[4]}" "${row[6]}" "$rss"
  done
done
CURRENT_LOG=""

# ============================================
# Résumé : médiane par version
# ============================================
echo ""
echo "═══════════════════════════════════════════════════"
echo "  MÉDIANES sur $ROUNDS tours"
echo "═══════════════════════════════════════════════════"
awk -F'\t' -v refs="${REFS[*]}" '
  function median(arr, n,   i, j, t) {
    for (i = 1; i <= n; i++) for (j = i + 1; j <= n; j++) if (arr[j] < arr[i]) { t = arr[i]; arr[i] = arr[j]; arr[j] = t }
    return (n % 2) ? arr[(n + 1) / 2] : (arr[n / 2] + arr[n / 2 + 1]) / 2
  }
  NR == 1 { for (c = 3; c <= NF; c++) hdr[c] = $c; ncol = NF; next }
  { r = $1; n[r]++; for (c = 3; c <= NF; c++) v[r, c, n[r]] = $c }
  END {
    nr = split(refs, R, " ")
    printf "%-14s", "metric"; for (i = 1; i <= nr; i++) printf " %14s", R[i]; if (nr == 2) printf " %9s", "delta"; printf "\n"
    for (c = 3; c <= ncol; c++) {
      printf "%-14s", hdr[c]
      for (i = 1; i <= nr; i++) {
        delete a; for (k = 1; k <= n[R[i]]; k++) a[k] = v[R[i], c, k] + 0
        m[i] = median(a, n[R[i]]); printf " %14.1f", m[i]
      }
      if (nr == 2 && m[1] != 0) printf " %+8.1f%%", (m[2] - m[1]) * 100 / m[1]
      printf "\n"
    }
  }' "$RESULTS"
echo ""
echo "Données brutes : $RESULTS   logs : $OUT_DIR/"
