#!/bin/bash
# Bench RadixOx contre Valkey (ou Redis, selon ce qui est installé) sur deux machines : ce PC fait le client (YCSB), la cible
# ssh fait le serveur. Tout est orchestré d'ici :
#
#   scripts/bench_2pc.sh server            # 1 cœur puis 3 cœurs physiques par serveur
#   scripts/bench_2pc.sh server 1 2 3      # les valeurs de K voulues
#
# Étapes : vérifications locales → mise à jour du dépôt distant (git pull, même commit
# qu'ici) → préparation du serveur en une session (sudo demandé une fois : réglages,
# services redis/valkey arrêtés, pare-feu ouvert à ce PC, build) → test de connexion →
# YCSB A contre RadixOx puis Valkey pour chaque K → tableaux.
#
# Variables : REMOTE_DIR (radixox), PORT (6379), RECORDS (1M), OPS (2M), THREADS (100),
# HOST (adresse du serveur ; défaut : celle que ssh résout pour la cible), SKIP_SETUP=1.
set -euo pipefail

TARGET="${1:?usage: $0 cible_ssh [K ...]   (ex. : $0 server 1 3)}"
shift
KS=("$@")
[ ${#KS[@]} -gt 0 ] || KS=(1 3)
PORT="${PORT:-6379}"
REMOTE_DIR="${REMOTE_DIR:-radixox}"
RECORDS="${RECORDS:-1000000}"
OPS="${OPS:-2000000}"
THREADS="${THREADS:-100}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
YCSB_DIR="$ROOT/../ycsb-redis-binding-0.18.0-SNAPSHOT"
OUT="${OUT:-$(mktemp -d /tmp/bench_2pc.XXXX)}"

step() { printf '\n\033[1m▶ %s\033[0m\n' "$*"; }
ok() { printf '  ✓ %s\n' "$*"; }
die() { printf '\n  ✗ %s\n' "$*" >&2; exit 1; }
remote() { ssh -o BatchMode=yes "$TARGET" "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh $*"; }

# ── 1. Ce PC ────────────────────────────────────────────────────────────────
step "Vérifications locales"
[ -x "$YCSB_DIR/bin/ycsb.sh" ] || die "YCSB introuvable dans $YCSB_DIR"
command -v java >/dev/null || die "java absent (YCSB en a besoin)"
command -v redis-cli >/dev/null || die "redis-cli absent"
ok "YCSB, java, redis-cli"
ssh -o BatchMode=yes -o ConnectTimeout=5 "$TARGET" true 2>/dev/null \
  || die "ssh $TARGET sans mot de passe impossible (ssh-copy-id $TARGET)"
ok "ssh $TARGET"
HOST="${HOST:-$(ssh -G "$TARGET" | awk '$1 == "hostname" {print $2; exit}')}"
CLIENT_IP=$(ip route get "$(getent ahostsv4 "$HOST" | awk '{print $1; exit}')" 2>/dev/null | grep -oP 'src \K\S+' || true)
[ -n "$CLIENT_IP" ] || die "impossible de déterminer l'adresse de ce PC vers $HOST"
ok "serveur $HOST, client $CLIENT_IP"

# ── 2. Dépôt distant ────────────────────────────────────────────────────────
step "Mise à jour du dépôt sur $TARGET"
LOCAL_REV=$(git -C "$ROOT" rev-parse --short HEAD)
if [ -n "$(git -C "$ROOT" log --oneline origin/main..HEAD 2>/dev/null)" ]; then
  echo "  ⚠ des commits locaux ne sont pas poussés : le serveur prendra origin/main"
fi
ssh -o BatchMode=yes "$TARGET" "cd $REMOTE_DIR && git pull --ff-only -q" || die "git pull impossible sur $TARGET:$REMOTE_DIR"
REMOTE_REV=$(ssh -o BatchMode=yes "$TARGET" "cd $REMOTE_DIR && git rev-parse --short HEAD")
[ "$REMOTE_REV" = "$LOCAL_REV" ] && ok "même commit des deux côtés ($LOCAL_REV)" \
  || echo "  ⚠ serveur sur $REMOTE_REV, ce PC sur $LOCAL_REV"

# ── 3. Préparation du serveur (sudo) ────────────────────────────────────────
if [ -z "${SKIP_SETUP:-}" ]; then
  step "Préparation de $TARGET (mot de passe sudo du serveur demandé une fois)"
  ssh -t "$TARGET" "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh setup $CLIENT_IP" \
    || die "préparation du serveur échouée (voir ci-dessus)"
fi

# ── 4. Test de connexion de bout en bout ────────────────────────────────────
step "Test de connexion"
trap 'remote stop >/dev/null 2>&1 || true' EXIT
remote start radixox 1 >/dev/null
if ! redis-cli -h "$HOST" -p "$PORT" PING 2>&1 | grep -q PONG; then
  die "le serveur tourne mais $HOST:$PORT ne répond pas depuis ce PC : $(redis-cli -h "$HOST" -p "$PORT" PING 2>&1)"
fi
remote stop >/dev/null
ok "$HOST:$PORT répond depuis ce PC"
REF=$(remote refname)
ok "référence : $REF"

# ── 5. Benchs ───────────────────────────────────────────────────────────────
ycsb() { # phase fichier
  local phase=$1 out=$2 count
  if [ "$phase" = load ]; then count=(); else count=(-p operationcount="$OPS"); fi
  "$YCSB_DIR/bin/ycsb.sh" "$phase" redis -s -P "$YCSB_DIR/workloads/workloada" \
    -p redis.host="$HOST" -p redis.port="$PORT" \
    -p fieldcount=1 -p fieldnamekey=false -p fieldlength=100 \
    -p recordcount="$RECORDS" "${count[@]}" -threads "$THREADS" \
    -p percentiles=95,99,99.9,99.99 >"$out" 2>&1 || die "YCSB $phase a échoué (log : $out)"
  # YCSB sort avec 0 même si toutes les opérations échouent : on vérifie.
  grep -q 'Return=OK' "$out" || die "YCSB $phase : aucune opération réussie (log : $out)"
  if grep -qE 'Return=(ERROR|NOT_FOUND)' "$out"; then
    echo "    ⚠ erreurs YCSB : $(grep -E 'Return=(ERROR|NOT_FOUND)' "$out" | tr '\n' ' ')"
  fi
}
stat() { grep -F "$2" "$1" 2>/dev/null | awk '{print $3}' | head -1 || true; }
pct() { grep -oP "\[$2:[^\]]*\]" "$1" 2>/dev/null | tail -1 | grep -oP "$3=\K[0-9]+" || true; }

declare -A R
bench() { # serveur K
  local s=$1 k=$2 f="$OUT/${1}_k$2"
  local started
  started=$(remote start "$s" "$k" 2>&1) || die "démarrage de $s ($k cœurs) impossible :
$started"
  echo "  $(echo "$started" | tail -1)"
  echo "    load de $RECORDS enregistrements…"
  ycsb load "${f}_load.txt"
  echo "    run de $OPS opérations sur les $RECORDS clés…"
  ycsb run "${f}_run.txt"
  R[$s,$k,load]=$(stat "${f}_load.txt" "Throughput(ops/sec)")
  R[$s,$k,loadp99]=$(stat "${f}_load.txt" "[INSERT], 99thPercentileLatency")
  R[$s,$k,run]=$(stat "${f}_run.txt" "Throughput(ops/sec)")
  R[$s,$k,avg]=$(stat "${f}_run.txt" "[READ], AverageLatency")
  R[$s,$k,p95]=$(stat "${f}_run.txt" "[READ], 95thPercentileLatency")
  R[$s,$k,p99]=$(stat "${f}_run.txt" "[READ], 99thPercentileLatency")
  R[$s,$k,p999]=$(pct "${f}_run.txt" READ 99.9)
  R[$s,$k,p9999]=$(pct "${f}_run.txt" READ 99.99)
  R[$s,$k,up99]=$(stat "${f}_run.txt" "[UPDATE], 99thPercentileLatency")
  R[$s,$k,rss]=$(remote rss)
  remote stop >/dev/null
}

row() { # k libellé clé plus_grand_gagne
  local a=${R[radixox,$1,$3]:-} b=${R[valkey,$1,$3]:-} w
  w=$(awk -v a="$a" -v b="$b" -v h="$4" -v ref="${REF%% *}" 'BEGIN { if (a=="" || b=="") print "N/A"; else if ((h && a+0>b+0) || (!h && a+0<b+0)) print "RadixOx"; else print ref }')
  printf "  %-24s %14.0f %14.0f %9s\n" "$2" "${a:-0}" "${b:-0}" "$w"
}

for k in "${KS[@]}"; do
  step "YCSB A, $k cœur(s) physique(s) par serveur"
  bench radixox "$k"
  bench valkey "$k"
done

step "Résultats — RadixOx contre $REF — $RECORDS enregistrements, $OPS opérations, $THREADS threads, serveur $TARGET ($HOST), commit $REMOTE_REV"
for k in "${KS[@]}"; do
  printf "\n  %-24s %14s %14s %9s\n" "" "RadixOx(${k}c)" "${REF%% *}(${k}t)" "gagnant"
  row "$k" "Load (ops/s)" load 1
  row "$k" "Load p99 (µs)" loadp99 0
  row "$k" "Run (ops/s)" run 1
  row "$k" "READ avg (µs)" avg 0
  row "$k" "READ p95 (µs)" p95 0
  row "$k" "READ p99 (µs)" p99 0
  row "$k" "READ p99.9 (µs)" p999 0
  row "$k" "READ p99.99 (µs)" p9999 0
  row "$k" "UPDATE p99 (µs)" up99 0
  row "$k" "Pic de RSS (Mo)" rss 0
done
echo ""
echo "  Logs : $OUT"
