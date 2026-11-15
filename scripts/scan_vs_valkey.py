"""SSCAN / HSCAN : scans complets, CPU du serveur (SCAN_DESIGN.md « Performances »).

Usage : python3 -I scripts/scan_vs_valkey.py <port> <pid du serveur> load|run [N] [reps]

  load : FLUSHALL, puis Set `s` (member:{i}) et Hash `h` (field:{i} -> v{i}), N = 1M
  run  : scan complet par cas (min de `reps`, défaut 3), puis SCARD x100k (aller-retour
         seul, pour soustraire le coût fixe d'un appel). CASES=0,4 : sous-ensemble des cas.

CPU = somme des `/proc/<pid>/task/*/schedstat` (ns), donc le serveur doit tourner sur la
même machine. Même client pour RadixOx et Valkey ; serveur épinglé (`taskset`), sous
`systemd-run --user --scope -p MemoryMax=3G`, profil CPU `performance`. Avant / après :
deux binaires chargés à neuf, passages alternés (ABBA). Exemple :

  RADIXOX_PORT=6391 taskset -c 2 target/release/radixox &
  valkey-server --port 6392 --save "" --appendonly no &
  python3 -I scripts/scan_vs_valkey.py 6391 $(pgrep -x radixox) load
  python3 -I scripts/scan_vs_valkey.py 6391 $(pgrep -x radixox) run
"""
import socket, sys, os, time

port, pid, mode = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
N = int(sys.argv[4]) if len(sys.argv) > 4 else 1_000_000
REPS = int(sys.argv[5]) if len(sys.argv) > 5 else 3

s = socket.create_connection(("127.0.0.1", port))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
f = s.makefile("rb", buffering=1 << 20)

def enc(args):
    return b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args)

def read():
    line = f.readline()
    t, rest = line[:1], line[1:-2]
    if t in (b"+", b":"):
        return rest
    if t == b"-":
        raise RuntimeError(rest)
    if t == b"$":
        n = int(rest)
        return None if n < 0 else f.read(n + 2)[:-2]
    if t == b"*":
        n = int(rest)
        return [read() for _ in range(n)]
    raise ValueError(line)

def cmd(*a):
    s.sendall(enc(a))
    return read()

def cpu_ns():
    tot = 0
    for t in os.listdir(f"/proc/{pid}/task"):
        with open(f"/proc/{pid}/task/{t}/schedstat") as h:
            tot += int(h.read().split()[0])
    return tot

if mode == "load":
    cmd(b"FLUSHALL")
    B = 1000
    for base in range(0, N, B):
        a = [b"SADD", b"s"] + [b"member:%d" % i for i in range(base, base + B)]
        h = [b"HSET", b"h"]
        for i in range(base, base + B):
            h += [b"field:%d" % i, b"v%d" % i]
        s.sendall(enc(a) + enc(h))
        read(); read()
    print("SCARD", cmd(b"SCARD", b"s"), "HLEN", cmd(b"HLEN", b"h"))
    sys.exit()

def full(c, key, count, match=None):
    cur, calls, seen = b"0", 0, 0
    while True:
        a = [c, key, cur, b"COUNT", b"%d" % count]
        if match:
            a += [b"MATCH", match]
        r = cmd(*a)
        calls += 1
        cur = r[0]
        seen += len(r[1])
        if cur == b"0":
            return calls, seen

cases = [
    (b"SSCAN", b"s", 1000, None),
    (b"HSCAN", b"h", 1000, None),
    (b"SSCAN", b"s", 1000, b"*:7*"),
    (b"SSCAN", b"s", 10, b"*:7*"),
    (b"SSCAN", b"s", 10, None),
    (b"HSCAN", b"h", 10, None),
]
sel = os.environ.get("CASES")
if sel:
    cases = [cases[int(i)] for i in sel.split(",")]
for c, key, count, match in cases:
    best = None
    for _ in range(REPS):
        c0, t0 = cpu_ns(), time.perf_counter()
        calls, seen = full(c, key, count, match)
        c1, t1 = cpu_ns(), time.perf_counter()
        r = ((c1 - c0) / 1e6, calls, seen, t1 - t0)
        if best is None or r[0] < best[0]:
            best = r
    ms, calls, seen, wall = best
    print(f"{c.decode()} COUNT {count} MATCH {match.decode() if match else '-':6} "
          f"cpu {ms:7.1f} ms  calls {calls:7d}  seen {seen:8d}  "
          f"{ms*1000/calls:6.2f} us/call  wall {wall:.2f}s")
# base aller-retour
c0 = cpu_ns()
for _ in range(100_000):
    cmd(b"SCARD", b"s")
print(f"SCARD x100k cpu {(cpu_ns()-c0)/1e6:.1f} ms")
