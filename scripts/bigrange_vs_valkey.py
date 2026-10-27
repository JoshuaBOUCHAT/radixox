"""LRANGE / ZRANGE géants (au-delà de RANGE_JOB_MIN : réponse par tranches) contre Valkey,
 et ordre des réponses en pipeline derrière une lecture parquée.

Usage : python3 -I scripts/bigrange_vs_valkey.py <port radixox> <port valkey>
"""
import socket, sys
def conn(p):
    s = socket.create_connection(("127.0.0.1", p)); return s, s.makefile("rb")
def send(c, *args):
    s, f = c
    s.sendall(b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args)); return read(f)
def read(f):
    line = f.readline(); t, rest = line[:1], line[1:-2]
    if t in (b"+", b"-", b":"): return (t, rest)
    if t == b"$":
        n = int(rest); return None if n < 0 else f.read(n + 2)[:-2]
    if t == b"*":
        n = int(rest); return None if n < 0 else [read(f) for _ in range(n)]
rx, vk = conn(int(sys.argv[1])), conn(int(sys.argv[2]))
for c in (rx, vk):
    send(c, b"FLUSHDB")
    for b in range(0, 50000, 500):
        send(c, b"RPUSH", b"bl", *[b"e%d" % i for i in range(b, b + 500)])
        send(c, b"ZADD", b"bz", *sum([[b"%d" % (i % 997), b"m%d" % i] for i in range(b, b + 500)], []))
cmds = [[b"LRANGE", b"bl", b"0", b"-1"], [b"LRANGE", b"bl", b"100", b"40000"], [b"LRANGE", b"bl", b"-30000", b"-1"],
        [b"ZRANGE", b"bz", b"0", b"-1", b"WITHSCORES"], [b"ZRANGE", b"bz", b"0", b"-1", b"REV"],
        [b"ZRANGE", b"bz", b"(100", b"+inf", b"BYSCORE", b"LIMIT", b"5", b"30000"], [b"ZREVRANGEBYSCORE", b"bz", b"+inf", b"200", b"WITHSCORES"],
        [b"ZRANGE", b"bz", b"10", b"20"]]
bad = 0
for cmd in cmds:
    a, b = send(rx, *cmd), send(vk, *cmd)
    ok = a == b; bad += not ok
    print(("OK  " if ok else "DIFF"), b" ".join(cmd).decode(), len(a) if isinstance(a, list) else a)
# ordre en pipeline : LRANGE géant puis PING sur la même connexion
s, f = rx
s.sendall(b"*4\r\n$6\r\nLRANGE\r\n$2\r\nbl\r\n$1\r\n0\r\n$2\r\n-1\r\n*1\r\n$4\r\nPING\r\n*2\r\n$4\r\nLLEN\r\n$2\r\nbl\r\n")
r1, r2, r3 = read(f), read(f), read(f)
print("pipeline :", len(r1), r2, r3)
print("différences :", bad)
