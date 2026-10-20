#!/bin/bash
# Côté serveur d'un bench réseau. Piloté par ssh depuis le client par bench_2pc.sh : rien
# à lancer à la main sur le serveur.
#
#   scripts/bench_server.sh check                     # prérequis (lecture seule, sans sudo)
#   scripts/bench_server.sh setup <ip_client>         # installe ce qui manque, prep, libère le
#                                                     # port, ouvre le pare-feu, build (sudo)
#   scripts/bench_server.sh firewall open|close <ip>  # pare-feu seul (ufw, firewalld ou iptables)
#   scripts/bench_server.sh prep                      # réglages système (sudo), une fois par démarrage
#   scripts/bench_server.sh start radixox [K]         # RadixOx sur K cœurs physiques (défaut 1)
#   scripts/bench_server.sh start valkey  [K]         # la référence sur les mêmes K cœurs :
#                                                     # valkey-server, ou redis-server s'il n'y en a pas
#   scripts/bench_server.sh stop | status | rss       # arrêt, état, pic de RSS (Mo)
#   scripts/bench_server.sh refname                   # nom et version de la référence
#
# Équité : un thread par cœur physique, les mêmes CPU pour les deux serveurs (les K
# premiers cœurs), les autres cœurs laissés au système et aux interruptions réseau.
#   RadixOx : boucle sur le 1er cœur, envois délégués à K-1 workers io-wq épinglés
#             sur les autres (RADIXOX_SEND_ASYNC) ;
#   Valkey  : --io-threads K --server-cpulist <mêmes CPU>.
# Les deux écoutent sur 0.0.0.0:$PORT sans mot de passe et sans persistance :
# n'ouvrir le port qu'à la machine cliente.
set -euo pipefail
# Pas de save automatique pendant un bench (Valkey y tourne avec --save "").
export RADIXOX_SAVE="${RADIXOX_SAVE-}"

# Session ssh non interactive : cargo n'est pas forcément dans le PATH.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-6379}"
STATE="${STATE:-/tmp/radixox-bench-server}"
PIDFILE="$STATE/server.pid"
mkdir -p "$STATE"

die() { echo "bench_server: $*" >&2; exit 1; }

# Root (conteneur, VM jetable) : pas besoin de sudo, qui peut d'ailleurs manquer.
if [ "$(id -u)" = 0 ]; then SUDO=(); else SUDO=(sudo); fi

version_ge() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$2" ]; }

# PONG sur 127.0.0.1:$PORT ? En bash pur (/dev/tcp) : redis-cli n'est pas requis ici.
ping_local() {
  (
    exec 3<>"/dev/tcp/127.0.0.1/$PORT" || exit 1
    printf 'PING\r\n' >&3
    read -r -t 1 reply <&3 || exit 1
    [ "${reply%$'\r'}" = "+PONG" ]
  ) 2>/dev/null
}

# Commit des sources : celui écrit par bench_2pc.sh à l'envoi, sinon git.
source_rev() {
  cat "$ROOT/.bench-rev" 2>/dev/null || git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '?'
}

# Serveur de référence : Valkey s'il est installé, sinon Redis (mêmes options).
ref_bin() {
  if command -v valkey-server >/dev/null; then echo valkey-server
  elif command -v redis-server >/dev/null; then echo redis-server
  else return 1
  fi
}

cmd_refname() {
  local bin; bin=$(ref_bin) || die "ni valkey-server ni redis-server"
  local v; v=$("$bin" --version | grep -oP 'v=\K[0-9.]+' || true)
  case "$bin" in valkey-server) echo "Valkey $v" ;; *) echo "Redis $v" ;; esac
}

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

# Processus qui écoute sur $PORT (vide si aucun).
port_owner() {
  ss -ltnpH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true
}

# Prêt quand c'est bien notre processus qui écoute et répond.
wait_ready() {
  local pid; pid=$(cat "$PIDFILE")
  for _ in $(seq 1 100); do
    running_pid >/dev/null || { tail -20 "$STATE/server.log" >&2; die "le serveur s'est arrêté au démarrage"; }
    if [ "$(port_owner)" = "$pid" ] && ping_local; then
      return 0
    fi
    sleep 0.1
  done
  die "pas de réponse de notre serveur sur :$PORT après 10 s (log : $STATE/server.log)"
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
  # Occupation du port testée sans le PID : `ss -p` ne montre pas les processus
  # d'un autre utilisateur (un service redis/valkey système, par exemple).
  if [ -n "$(ss -ltnH "sport = :$PORT" 2>/dev/null)" ]; then
    die "port $PORT déjà pris ($(ss -ltnH "sport = :$PORT" | awk '{print $4}' | paste -sd' ')) — service système ? (sudo systemctl disable --now redis-server ou valkey-server)"
  fi
  rm -f "$STATE"/dump.rxd*
  ulimit -n 65536 2>/dev/null || true
  case "$which" in
    radixox)
      local bin="$ROOT/target/release/radixox"
      # Toujours à jour avec le dépôt (rapide si rien n'a changé).
      # Sortie dans build.log : cargo rejouerait ses warnings à chaque démarrage.
      (cd "$ROOT" && RUSTFLAGS="-C target-cpu=native" cargo build --release --bin radixox) >"$STATE/build.log" 2>&1 \
        || { tail -30 "$STATE/build.log" >&2; die "build de radixox impossible (log : $STATE/build.log)"; }
      local env=(RADIXOX_BIND=0.0.0.0 RADIXOX_PORT="$PORT" RADIXOX_CPU_PIN="$main" RADIXOX_DIR="$STATE")
      if [ -n "$iowq" ]; then
        env+=(RADIXOX_SEND_ASYNC=1 RADIXOX_IOWQ_WORKERS=$((k - 1)) RADIXOX_IOWQ_CPUS="$iowq")
      fi
      # Détaché (setsid, stdin fermé) : une session ssh qui lance le script n'attend pas le serveur.
      setsid taskset -c "$cpus" env "${env[@]}" "$bin" </dev/null >"$STATE/server.log" 2>&1 &
      ;;
    valkey)
      local bin; bin=$(ref_bin) || die "ni valkey-server ni redis-server"
      # Redis 7.0 : `server_cpulist` (tiret bas) ; et ses io-threads ne lisent le
      # réseau qu'avec io-threads-do-reads. Valkey accepte `server-cpulist`.
      local extra=() cpulist_opt=--server-cpulist
      if [ "$bin" = redis-server ]; then
        cpulist_opt=--server_cpulist
        [ "$k" -gt 1 ] && extra=(--io-threads-do-reads yes)
      fi
      setsid taskset -c "$cpus" "$bin" --port "$PORT" --bind 0.0.0.0 --protected-mode no \
        --save "" --appendonly no --dir "$STATE" \
        --io-threads "$k" "$cpulist_opt" "$cpus" "${extra[@]}" </dev/null >"$STATE/server.log" 2>&1 &
      which="$(cmd_refname)"
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
    ping_local && echo PONG || echo "pas de réponse sur :$PORT"
  else
    echo "aucun serveur lancé par ce script"
  fi
}

cmd_rss() {
  local pid
  pid=$(running_pid) || die "aucun serveur lancé"
  awk '/VmHWM/ {printf "%.0f\n", $2 / 1024}' "/proc/$pid/status"
}

# Lecture seule, sans sudo. Ce qui manque et que `setup` installe est seulement
# annoncé ; code 2 pour ce qu'aucun script ne peut corriger (noyau, pas de sudo).
cmd_check() {
  local fatal=() todo=() c
  version_ge "$(uname -r | cut -d. -f1-2)" 6.12 \
    || fatal+=("noyau >= 6.12 pour io_uring (actuel : $(uname -r) ; Ubuntu 24.04 : sudo apt install linux-generic-hwe-24.04 puis redémarrer)")
  [ ${#SUDO[@]} -eq 0 ] || command -v sudo >/dev/null || fatal+=("sudo absent : se connecter en root")
  [ "$(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo 0)" = 0 ] || todo+=("io_uring à réactiver")
  rust_ok || todo+=("Rust >= $RUST_MIN (rustup)")
  for c in cc curl tar lscpu taskset setsid ss; do
    command -v "$c" >/dev/null || todo+=("$c")
  done
  command -v valkey-server >/dev/null || todo+=("valkey (à défaut : redis)")
  if [ ${#todo[@]} -gt 0 ] && ! pkg_manager >/dev/null; then
    fatal+=("ni apt, ni pacman, ni dnf : installer à la main : ${todo[*]}")
  fi
  [ ${#fatal[@]} -eq 0 ] || { printf '  ✗ %s\n' "${fatal[@]}" >&2; return 2; }
  [ ${#todo[@]} -eq 0 ] || printf '  • à installer/régler par la préparation : %s\n' "${todo[*]}"
  echo "  ✓ $(uname -r), $(lscpu -p=CORE 2>/dev/null | grep -v '^#' | sort -u | wc -l) cœurs physiques"
}

RUST_MIN=1.88
rust_ok() {
  command -v cargo >/dev/null && version_ge "$(cargo --version | awk '{print $2}')" "$RUST_MIN"
}

pkg_manager() {
  local m
  for m in apt-get pacman dnf; do command -v "$m" >/dev/null && { echo "$m"; return 0; }; done
  return 1
}

# pkg_install <nom apt> <nom pacman> <nom dnf>
pkg_install() {
  case "$(pkg_manager)" in
    apt-get)
      if [ -z "${APT_UPDATED:-}" ]; then "${SUDO[@]}" apt-get update -qq || true; APT_UPDATED=1; fi
      "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$1" >/dev/null ;;
    pacman) "${SUDO[@]}" pacman -S --needed --noconfirm "$2" >/dev/null ;;
    dnf) "${SUDO[@]}" dnf install -y -q "$3" >/dev/null ;;
    *) return 1 ;;
  esac
}

# Tout ce qui manque : paquets, Rust, Valkey (Redis à défaut).
install_deps() {
  command -v cc >/dev/null || { echo "installation : compilateur C"; pkg_install build-essential base-devel gcc; }
  command -v curl >/dev/null || { echo "installation : curl"; pkg_install curl curl curl; }
  command -v tar >/dev/null || pkg_install tar tar tar
  local c
  for c in lscpu taskset setsid; do
    command -v "$c" >/dev/null || { echo "installation : util-linux"; pkg_install util-linux util-linux util-linux; break; }
  done
  command -v ss >/dev/null || { echo "installation : iproute2"; pkg_install iproute2 iproute2 iproute; }
  if ! command -v valkey-server >/dev/null; then
    echo "installation : valkey"
    pkg_install valkey-server valkey valkey 2>/dev/null \
      || { command -v redis-server >/dev/null || { echo "valkey indisponible, installation : redis"; pkg_install redis-server redis redis; }; }
  fi
  if ! rust_ok; then
    if command -v rustup >/dev/null; then
      echo "mise à jour de Rust (rustup)"
      rustup update stable --no-self-update >/dev/null
    else
      echo "installation de Rust (rustup, dans ~/.cargo)"
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    fi
    . "$HOME/.cargo/env"
  fi
  if [ "$(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo 0)" != 0 ]; then
    "${SUDO[@]}" sysctl -q kernel.io_uring_disabled=0 && echo "io_uring réactivé"
  fi
}

# Pare-feu : open|close <ip_client>. ufw, sinon firewalld, sinon iptables s'il filtre.
cmd_firewall() {
  local op=${1:?firewall open|close <ip_client>} client=${2:?ip_client}
  # LC_ALL=C : sinon « État : actif » sur un système en français.
  if command -v ufw >/dev/null && "${SUDO[@]}" env LC_ALL=C ufw status | grep -q '^Status: active'; then
    if [ "$op" = open ]; then
      "${SUDO[@]}" ufw allow from "$client" to any port "$PORT" proto tcp >/dev/null
    else
      "${SUDO[@]}" ufw delete allow from "$client" to any port "$PORT" proto tcp >/dev/null
    fi
    echo "pare-feu (ufw) : port $PORT $([ "$op" = open ] && echo ouvert || echo refermé) pour $client"
  elif systemctl is-active --quiet firewalld 2>/dev/null; then
    # Règle runtime seulement : disparaît aussi au prochain reload/redémarrage.
    "${SUDO[@]}" firewall-cmd -q "--$([ "$op" = open ] && echo add || echo remove)-rich-rule=rule family=ipv4 source address=$client port port=$PORT protocol=tcp accept" || true
    echo "pare-feu (firewalld) : port $PORT $([ "$op" = open ] && echo ouvert || echo refermé) pour $client"
  elif command -v iptables >/dev/null \
       && "${SUDO[@]}" iptables -S INPUT 2>/dev/null | grep -qE '^-P INPUT DROP|-j (DROP|REJECT)'; then
    local rule=(INPUT -p tcp -s "$client" --dport "$PORT" -j ACCEPT)
    if [ "$op" = open ]; then
      "${SUDO[@]}" iptables -C "${rule[@]}" 2>/dev/null || "${SUDO[@]}" iptables -I "${rule[@]}"
    else
      "${SUDO[@]}" iptables -D "${rule[@]}" 2>/dev/null || true
    fi
    echo "pare-feu (iptables) : port $PORT $([ "$op" = open ] && echo ouvert || echo refermé) pour $client"
  fi
}

cmd_prep() {
  echo "noyau : $(uname -r)"
  echo "io_uring_disabled : $(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo '?') (doit valoir 0)"
  echo "cœurs physiques : $(lscpu -p=CORE | grep -v '^#' | sort -u | wc -l), CPU : $(nproc)"
  # Fréquence : gouverneur performance sur tous les CPU.
  if command -v powerprofilesctl >/dev/null; then
    powerprofilesctl set performance && echo "profil d'énergie : performance"
  fi
  for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    [ -f "$g" ] && echo performance | "${SUDO[@]}" tee "$g" >/dev/null
  done
  echo "gouverneur : $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo '?')"
  # THP en madvise : RadixOx le demande pour son arbre, Valkey ne le prend pas (sa recommandation).
  echo madvise | "${SUDO[@]}" tee /sys/kernel/mm/transparent_hugepage/enabled >/dev/null
  echo "THP : $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
  "${SUDO[@]}" sysctl -q vm.overcommit_memory=1 && echo "vm.overcommit_memory = 1 (recommandé par Valkey)"
  local iface
  iface=$(ip route show default 2>/dev/null | awk '{print $5; exit}')
  if [ -n "$iface" ]; then
    echo "interface $iface : $(cat "/sys/class/net/$iface/speed" 2>/dev/null || echo '?') Mb/s"
  fi
}

# Tout ce qui demande sudo, en une fois : installations, réglages, port libéré,
# pare-feu ouvert au client, build.
cmd_setup() {
  local client=${1:?setup <ip_client>}
  cmd_check >/dev/null || die "prérequis impossibles à corriger automatiquement (voir ci-dessus)"
  install_deps
  cmd_prep
  # Services redis / valkey du système (dont ceux qu'on vient d'installer) qui prendraient le port.
  local svc
  for svc in $(systemctl list-units --type=service --state=active --no-legend 2>/dev/null \
                 | awk '{print $1}' | grep -Ei '^(redis|valkey)' || true); do
    echo "service système $svc arrêté et désactivé (il occupe le port des benchs)"
    "${SUDO[@]}" systemctl disable --now "$svc" 2>/dev/null
  done
  cmd_stop >/dev/null
  if [ -n "$(ss -ltnH "sport = :$PORT" 2>/dev/null)" ]; then
    die "port $PORT toujours pris : $(ss -ltnH "sport = :$PORT" | awk '{print $4}' | paste -sd' ') (PORT=autre_port)"
  fi
  cmd_firewall open "$client"
  echo "référence : $(cmd_refname)"
  echo "build de radixox (plusieurs minutes la première fois : LTO)…"
  # Progression visible (une ligne par crate), sortie complète dans build.log.
  (cd "$ROOT" && RUSTFLAGS="-C target-cpu=native" cargo build --release --bin radixox 2>&1 \
     | tee "$STATE/build.log" | grep --line-buffered -E '^\s*(Compiling|Finished)'; exit "${PIPESTATUS[0]}") \
    || { tail -30 "$STATE/build.log" >&2; die "build impossible (log : $STATE/build.log)"; }
  echo "serveur prêt (commit $(source_rev))"
}

case "${1:-}" in
  check) cmd_check ;;
  firewall) shift; cmd_firewall "$@" ;;
  setup) shift; cmd_setup "$@" ;;
  prep) cmd_prep ;;
  start) shift; cmd_start "$@" ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  rss) cmd_rss ;;
  refname) cmd_refname ;;
  *) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
