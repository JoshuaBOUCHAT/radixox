#!/bin/bash
# Côté serveur d'un bench réseau (à lancer sur la machine serveur, dans le dépôt).
#
#   scripts/bench_server.sh prep                      # réglages système (sudo), une fois par démarrage
#   scripts/bench_server.sh start radixox [K]         # RadixOx sur K cœurs physiques (défaut 1)
#   scripts/bench_server.sh start valkey  [K]         # Valkey sur les mêmes K cœurs
#   scripts/bench_server.sh stop | status | rss       # arrêt, état, pic de RSS (Mo)
#
# Équité : un thread par cœur physique, les mêmes CPU pour les deux serveurs (les K
# premiers cœurs), les autres cœurs laissés au système et aux interruptions réseau.
#   RadixOx : boucle sur le 1er cœur, envois délégués à K-1 workers io-wq épinglés
#             sur les autres (RADIXOX_SEND_ASYNC) ;
#   Valkey  : --io-threads K --server-cpulist <mêmes CPU>.
# Les deux écoutent sur 0.0.0.0:$PORT sans mot de passe et sans persistance :
# n'ouvrir le port qu'à la machine cliente.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-6379}"
STATE="${STATE:-/tmp/radixox-bench-server}"
PIDFILE="$STATE/server.pid"
mkdir -p "$STATE"

die() { echo "bench_server: $*" >&2; exit 1; }

# Premier CPU de chacun des K premiers cœurs physiques, séparés par des virgules.
server_cpus() {
  local k=$1
  local cpus
  cpus=$(lscpu -p=CPU,CORE | grep -v '^#' | sort -t, -k2,2n -k1,1n | awk -F, '!seen[$2]++ {print $1}')
  local ncores
  ncores=$(echo "$cpus" | wc -l)
  [ "$k" -ge 1 ] || die "K doit valoir au moins 1"
  if [ "$k" -ge "$ncores" ] && [ -z "${ALLOW_ALL_CORES:-}" ]; then
    die "K=$k : $ncores cœurs physiques, garder au moins un cœur pour le système (ALLOW_ALL_CORES=1 pour forcer)"
  fi
  echo "$cpus" | head -n "$k" | paste -sd,
}

running_pid() {
  [ -f "$PIDFILE" ] || return 1
  local pid
  pid=$(cat "$PIDFILE")
  kill -0 "$pid" 2>/dev/null && echo "$pid"
}

wait_ready() {
  for _ in $(seq 1 100); do
    redis-cli -p "$PORT" PING 2>/dev/null | grep -q PONG && return 0
    running_pid >/dev/null || { tail -20 "$STATE/server.log" >&2; die "le serveur s'est arrêté au démarrage"; }
    sleep 0.1
  done
  die "pas de réponse sur :$PORT après 10 s"
}

cmd_stop() {
  local pid
  if pid=$(running_pid); then
    kill "$pid"
    for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$pid" 2>/dev/null || true
    echo "arrêté (pid $pid)"
  fi
  rm -f "$PIDFILE"
}

cmd_start() {
  local which=${1:-} k=${2:-1}
  local cpus main iowq
  cpus=$(server_cpus "$k")
  main=${cpus%%,*}
  iowq=${cpus#"$main"}; iowq=${iowq#,}
  cmd_stop >/dev/null
  rm -f "$STATE"/dump.rxd*
  ulimit -n 65536 2>/dev/null || true
  case "$which" in
    radixox)
      local bin="$ROOT/target/release/radixox"
      if [ ! -x "$bin" ]; then
        echo "build de radixox (target-cpu=native)…"
        (cd "$ROOT" && RUSTFLAGS="-C target-cpu=native" cargo build --release --bin radixox)
      fi
      local env=(RADIXOX_BIND=0.0.0.0 RADIXOX_PORT="$PORT" RADIXOX_CPU_PIN="$main" RADIXOX_DIR="$STATE")
      if [ -n "$iowq" ]; then
        env+=(RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS=$((k - 1)) RADIXOX_IOWQ_CPUS="$iowq")
      fi
      # Détaché (setsid, stdin fermé) : une session ssh qui lance le script n'attend pas le serveur.
      setsid taskset -c "$cpus" env "${env[@]}" "$bin" </dev/null >"$STATE/server.log" 2>&1 &
      ;;
    valkey)
      command -v valkey-server >/dev/null || die "valkey-server introuvable"
      setsid taskset -c "$cpus" valkey-server --port "$PORT" --bind 0.0.0.0 --protected-mode no \
        --save "" --appendonly no --dir "$STATE" \
        --io-threads "$k" --server-cpulist "$cpus" </dev/null >"$STATE/server.log" 2>&1 &
      ;;
    *) die "start radixox|valkey [K]" ;;
  esac
  echo $! >"$PIDFILE"
  wait_ready
  echo "$which démarré : $k cœur(s), CPU $cpus${iowq:+ (boucle sur $main, délégation sur $iowq)}, port $PORT, pid $(cat "$PIDFILE")"
}

cmd_status() {
  local pid
  if pid=$(running_pid); then
    ps -o pid,psr,pcpu,rss,args -p "$pid"
    redis-cli -p "$PORT" PING
  else
    echo "aucun serveur lancé par ce script"
  fi
}

cmd_rss() {
  local pid
  pid=$(running_pid) || die "aucun serveur lancé"
  awk '/VmHWM/ {printf "%.0f\n", $2 / 1024}' "/proc/$pid/status"
}

cmd_prep() {
  echo "noyau : $(uname -r)"
  local v; v=$(uname -r | cut -d. -f1-2)
  if [ "$(printf '%s\n6.12\n' "$v" | sort -V | head -1)" != "6.12" ]; then
    echo "  ⚠ RadixOx demande un noyau >= 6.12 (Ubuntu 24.04 : linux-generic-hwe-24.04)"
  fi
  echo "io_uring_disabled : $(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo '?') (doit valoir 0)"
  echo "cœurs physiques : $(lscpu -p=CORE | grep -v '^#' | sort -u | wc -l), CPU : $(nproc)"
  # Fréquence : gouverneur performance sur tous les CPU.
  if command -v powerprofilesctl >/dev/null; then
    powerprofilesctl set performance && echo "profil d'énergie : performance"
  fi
  for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    [ -f "$g" ] && echo performance | sudo tee "$g" >/dev/null
  done
  echo "gouverneur : $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo '?')"
  # THP en madvise : RadixOx le demande pour son arbre, Valkey ne le prend pas (sa recommandation).
  echo madvise | sudo tee /sys/kernel/mm/transparent_hugepage/enabled >/dev/null
  echo "THP : $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
  sudo sysctl -q vm.overcommit_memory=1 && echo "vm.overcommit_memory = 1 (recommandé par Valkey)"
  local iface
  iface=$(ip route show default 2>/dev/null | awk '{print $5; exit}')
  if [ -n "$iface" ]; then
    echo "interface $iface : $(cat "/sys/class/net/$iface/speed" 2>/dev/null || echo '?') Mb/s"
  fi
}

case "${1:-}" in
  prep) cmd_prep ;;
  start) shift; cmd_start "$@" ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  rss) cmd_rss ;;
  *) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
