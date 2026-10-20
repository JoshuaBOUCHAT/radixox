#!/bin/bash
# Bench RadixOx contre Valkey (ou Redis, selon ce qui est installé) sur deux machines : ce PC fait le client (YCSB), la cible
# ssh fait le serveur. Tout est orchestré d'ici, sur n'importe quelle machine Linux joignable
# en ssh sans mot de passe (clé) : rien à cloner ni à installer à la main côté serveur
# hormis les prérequis listés par l'étape de vérification.
#
#   scripts/bench_2pc.sh server            # 1 cœur puis 3 cœurs physiques par serveur
#   scripts/bench_2pc.sh user@10.0.0.5 2   # n'importe quelle cible ssh, les valeurs de K voulues
#
# Étapes : vérifications locales → envoi des sources d'ici (fichiers suivis par git, modifs
# non commitées comprises) dans un dossier dédié du serveur → vérification des prérequis
# distants → préparation du serveur en une session (sudo demandé une fois s'il n'est pas
# sans mot de passe) : installation de ce qui manque (compilateur C, Rust via rustup,
# Valkey ou à défaut Redis, outils système), réglages, services redis/valkey arrêtés,
# port ouvert à ce PC (ufw, firewalld ou iptables), build → test de connexion → YCSB A
# contre RadixOx puis Valkey pour chaque K → tableaux → port refermé.
#
# Variables : REMOTE_DIR (.cache/radixox-bench, relatif au home distant ; dossier réservé
# au bench, vidé à chaque envoi sauf target/), PORT (6379), RECORDS (1M), OPS (2M),
# THREADS (100), HOST (adresse du serveur ; défaut : celle que ssh résout pour la cible),
# YCSB_DIR (../ycsb-redis-binding-0.18.0-SNAPSHOT), SKIP_SETUP=1, FORCE=1 (lancer sans
# confirmation malgré un lien lent : Wi-Fi ou RTT > 1 ms).
set -euo pipefail

TARGET="${1:?usage: $0 cible_ssh [K ...]   (ex. : $0 server 1 3)}"
shift
KS=("$@")
[ ${#KS[@]} -gt 0 ] || KS=(1 3)
PORT="${PORT:-6379}"
REMOTE_DIR="${REMOTE_DIR:-.cache/radixox-bench}"
RECORDS="${RECORDS:-1000000}"
OPS="${OPS:-2000000}"
THREADS="${THREADS:-100}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
YCSB_DIR="${YCSB_DIR:-$ROOT/../ycsb-redis-binding-0.18.0-SNAPSHOT}"
OUT="${OUT:-$(mktemp -d /tmp/bench_2pc.XXXX)}"
# Une seule connexion ssh partagée par tous les appels (authentification faite une fois).
SSH=(ssh -o ControlMaster=auto -o "ControlPath=$OUT/ssh-%C" -o ControlPersist=120)

step() { printf '\n\033[1m▶ %s\033[0m\n' "$*"; }
ok() { printf '  ✓ %s\n' "$*"; }
die() { printf '\n  ✗ %s\n' "$*" >&2; exit 1; }
rsh() { "${SSH[@]}" -o BatchMode=yes "$TARGET" "$@"; }
# Pour sudo : terminal distant si on en a un (mot de passe), sinon sudo sans mot de passe.
rsudo() { if [ -t 0 ]; then "${SSH[@]}" -t "$TARGET" "$@"; else rsh "$@"; fi; }
remote() { rsh "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh $*"; }

SERVER_UP= FW_OPEN=
cleanup() {
  [ -z "$SERVER_UP" ] || remote stop >/dev/null 2>&1 || true
  # Le port ouvert par la préparation est refermé, même après un échec ou Ctrl-C.
  [ -z "$FW_OPEN" ] || rsudo \
    "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh firewall close $CLIENT_IP" 2>/dev/null || true
  "${SSH[@]}" -O exit "$TARGET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── 1. Ce PC ────────────────────────────────────────────────────────────────
step "Vérifications locales"
[ -x "$YCSB_DIR/bin/ycsb.sh" ] || die "YCSB introuvable dans $YCSB_DIR"
command -v java >/dev/null || die "java absent (YCSB en a besoin)"
command -v redis-cli >/dev/null || die "redis-cli absent"
ok "YCSB, java, redis-cli"
rsh -o ConnectTimeout=5 true 2>/dev/null \
  || die "ssh $TARGET sans mot de passe impossible (ssh-copy-id $TARGET)"
ok "ssh $TARGET"
HOST="${HOST:-$(ssh -G "$TARGET" | awk '$1 == "hostname" {print $2; exit}')}"
SERVER_IP=$(getent ahostsv4 "$HOST" | awk '{print $1; exit}')
[ -n "$SERVER_IP" ] || die "impossible de résoudre $HOST en IPv4 (HOST=<ip> pour forcer)"
CLIENT_IP=$(ip route get "$SERVER_IP" 2>/dev/null | grep -oP 'src \K\S+' || true)
[ -n "$CLIENT_IP" ] || die "impossible de déterminer l'adresse de ce PC vers $HOST"
ok "serveur $HOST ($SERVER_IP), client $CLIENT_IP"

# ── 2. Sources envoyées d'ici ───────────────────────────────────────────────
step "Envoi des sources vers $TARGET:~/$REMOTE_DIR"
REV=$(git -C "$ROOT" rev-parse --short HEAD)
[ -z "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ] || REV="$REV+modifs"
# Le dossier distant est vidé (sauf target/, pour un build incrémental) : on refuse d'y
# toucher s'il n'a pas été créé par ce script (un clone de travail, par exemple).
rsh "set -e; mkdir -p $REMOTE_DIR; cd $REMOTE_DIR
     if [ -n \"\$(ls -A)\" ] && [ ! -f .radixox-bench ]; then
       echo \"~/$REMOTE_DIR existe et n'est pas un dossier de bench (REMOTE_DIR=autre_dossier)\" >&2; exit 1
     fi" || die "dossier distant inutilisable"
(cd "$ROOT" && git ls-files -z | while IFS= read -r -d '' f; do
   [ -e "$f" ] && printf '%s\0' "$f"; done | tar --null -T - -czf -) \
  | rsh "set -e; cd $REMOTE_DIR
         find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
         tar -xzf -; touch .radixox-bench; echo $REV >.bench-rev" \
  || die "envoi des sources impossible"
ok "sources de $REV envoyées"

# ── 3. Prérequis distants (lecture seule) ───────────────────────────────────
step "Prérequis de $TARGET"
remote check || die "$TARGET ne peut pas servir de serveur de bench (voir ci-dessus)"

# ── 4. Préparation du serveur (sudo) ────────────────────────────────────────
if [ -z "${SKIP_SETUP:-}" ]; then
  step "Préparation de $TARGET : installations, réglages, pare-feu, build (sudo du serveur, une fois)"
  FW_OPEN=1
  rsudo "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh setup $CLIENT_IP" \
    || die "préparation du serveur échouée (voir ci-dessus)"
else
  # Le port est refermé à la fin de chaque bench : on le rouvre même sans préparation.
  FW_OPEN=1
  rsudo "cd $REMOTE_DIR && PORT=$PORT scripts/bench_server.sh firewall open $CLIENT_IP" \
    || die "ouverture du port impossible sur $TARGET"
fi

# ── 5. Test de connexion de bout en bout ────────────────────────────────────
step "Test de connexion"
SERVER_UP=1
remote start radixox 1 >/dev/null
# Avec timeout : un pare-feu qui jette les paquets laisserait redis-cli attendre indéfiniment.
if ! PONG=$(timeout 5 redis-cli -h "$HOST" -p "$PORT" PING 2>&1) || [ "$PONG" != PONG ]; then
  die "le serveur tourne sur $TARGET mais $HOST:$PORT ne répond pas depuis ce PC (${PONG:-rien en 5 s}) : pare-feu du serveur ?"
fi
remote stop >/dev/null
ok "$HOST:$PORT répond depuis ce PC"
REF=$(remote refname)
ok "référence : $REF"

# ── 6. Réseau ───────────────────────────────────────────────────────────────
# Un lien lent (Wi-Fi, quelques ms de RTT) borne le débit à THREADS / RTT : le bench
# mesure alors le réseau, pas les serveurs, et dure des heures aux volumes par défaut.
step "Réseau"
wifi() { [ -n "$1" ] && [ -d "/sys/class/net/$1/wireless" ]; }
LOCAL_IF=$(ip route get "$SERVER_IP" | grep -oP 'dev \K\S+')
REMOTE_IF=$(rsh "ip route get $CLIENT_IP | grep -oP 'dev \K\S+'; true")
RTT=$(ping -c 10 -i 0.2 -q "$SERVER_IP" 2>/dev/null | awk -F/ '/^(rtt|round-trip)/ {print $5}' || true)
ok "RTT moyen ${RTT:-?} ms, interface $LOCAL_IF ici, $REMOTE_IF sur $TARGET"
SLOW=()
wifi "$LOCAL_IF" && SLOW+=("ce PC est en Wi-Fi ($LOCAL_IF)")
rsh "[ -d /sys/class/net/$REMOTE_IF/wireless ]" && SLOW+=("$TARGET est en Wi-Fi ($REMOTE_IF)")
[ -n "$RTT" ] && awk -v r="$RTT" 'BEGIN { exit !(r > 1) }' && SLOW+=("RTT de $RTT ms (lien filaire local : ~0,1-0,3 ms)")
TOTAL=$(( (RECORDS + OPS) * 2 * ${#KS[@]} ))
echo "  volume : $TOTAL opérations au total (load $RECORDS + run $OPS, × 2 serveurs × ${#KS[@]} valeur(s) de K)"
if [ ${#SLOW[@]} -gt 0 ]; then
  printf '  ⚠ %s\n' "${SLOW[@]}"
  echo "  ⚠ le résultat mesurera surtout le réseau, et au volume ci-dessus ça peut durer des heures"
  echo "    (câble Ethernet, ou volumes réduits : RECORDS=20000 OPS=40000)"
  if [ -z "${FORCE:-}" ]; then
    [ -t 0 ] || die "lien lent : arrêt (FORCE=1 pour lancer quand même)"
    read -r -p "  Lancer quand même ? [o/N] " a
    [[ $a =~ ^[oOyY] ]] || die "arrêté"
  fi
fi

# ── 7. Benchs ───────────────────────────────────────────────────────────────
# Ligne d'état de YCSB (toutes les 10 s) réécrite sur place.
progress() {
  local l re='([0-9]+) sec: ([0-9]+) operations; ([0-9]+)[.,0-9]* current ops/sec(; est completion in ([^[]*[^[ ]))?'
  while IFS= read -r l; do
    [[ $l =~ $re ]] || continue
    printf '\r\033[K      %s s : %s opérations, %s ops/s%s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" \
      "${BASH_REMATCH[3]}" "${BASH_REMATCH[5]:+, fin estimée dans ${BASH_REMATCH[5]}}"
  done
  printf '\r\033[K'
}

ycsb() { # phase fichier
  local phase=$1 out=$2 count
  if [ "$phase" = load ]; then count=(); else count=(-p operationcount="$OPS"); fi
  # Code de retour par fichier : le pipeline de progression le masquerait.
  { "$YCSB_DIR/bin/ycsb.sh" "$phase" redis -s -P "$YCSB_DIR/workloads/workloada" \
      -p redis.host="$HOST" -p redis.port="$PORT" \
      -p fieldcount=1 -p fieldnamekey=false -p fieldlength=100 \
      -p recordcount="$RECORDS" "${count[@]}" -threads "$THREADS" \
      -p percentiles=95,99,99.9,99.99 2>&1; echo $? >"$out.rc"; } | tee "$out" | progress
  [ "$(cat "$out.rc")" = 0 ] || die "YCSB $phase a échoué (log : $out)"
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
  # LC_NUMERIC=C : avec une locale française, printf refuse « 669.03 » (il attend une virgule).
  LC_NUMERIC=C printf "  %-24s %14.0f %14.0f %9s\n" "$2" "${a:-0}" "${b:-0}" "$w"
}

for k in "${KS[@]}"; do
  step "YCSB A, $k cœur(s) physique(s) par serveur"
  bench radixox "$k"
  bench valkey "$k"
done

step "Résultats — RadixOx contre $REF — $RECORDS enregistrements, $OPS opérations, $THREADS threads, serveur $TARGET ($HOST), commit $REV"
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
