"""Différentiel List : mêmes commandes à RadixOx et à Valkey, réponses comparées.

Usage : python3 -I scripts/list_vs_valkey.py <port radixox> <port valkey> [graine] [commandes] [big]
(`big` : chaque clé part de 2000 éléments, plusieurs blocs). Base vidée au départ (FLUSHDB).
Les commandes bloquantes ne sont envoyées qu'avec un timeout court sur des clés non vides
ou en non-bloquant ; leur attente est testée à part (tests d'intégration).
"""
import random, socket, sys

def conn(port):
    s = socket.create_connection(("127.0.0.1", port))
    return s, s.makefile("rb")

def send(c, args):
    s, f = c
    s.sendall(b"*%d\r\n" % len(args) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in args))
    return read(f)

def read(f):
    line = f.readline()
    t, rest = line[:1], line[1:-2]
    if t in (b"+", b"-", b":"):
        return (t, rest if t != b"-" else rest.split(b" ")[0])
    if t == b"$":
        n = int(rest)
        if n < 0:
            return "nil"
        return f.read(n + 2)[:-2]
    if t == b"*":
        n = int(rest)
        return "nil*" if n < 0 else [read(f) for _ in range(n)]
    raise ValueError(line)

rx, vk = conn(int(sys.argv[1])), conn(int(sys.argv[2]))
rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 1)
for c in (rx, vk):
    send(c, [b"FLUSHDB"])
keys = [b"la", b"lb", b"lc"]
def val():
    return b"v%d" % rng.randrange(300 if rng.random() < 0.5 else 8)
def idx():
    return b"%d" % rng.randrange(-40, 40)
def side():
    return rng.choice([b"LEFT", b"RIGHT", b"left"])
if len(sys.argv) > 5:
    for k in keys:
        for b in range(0, 2000, 100):
            args = [b"RPUSH", k] + [b"v%d" % ((b + i) % 50) for i in range(100)]
            for c in (rx, vk):
                send(c, args)
ops = 0
mismatch = 0
for step in range(int(sys.argv[4]) if len(sys.argv) > 4 else 20000):
    k = rng.choice(keys)
    r = rng.randrange(100)
    if r < 18: cmd = [rng.choice([b"LPUSH", b"RPUSH"]), k] + [val() for _ in range(rng.randrange(1, 6))]
    elif r < 21: cmd = [rng.choice([b"LPUSHX", b"RPUSHX"]), k, val()]
    elif r < 31: cmd = [rng.choice([b"LPOP", b"RPOP"]), k] + ([b"%d" % rng.randrange(0, 6)] if rng.random() < .4 else [])
    elif r < 34: cmd = [b"LLEN", k]
    elif r < 42: cmd = [b"LRANGE", k, idx(), idx()]
    elif r < 47: cmd = [b"LINDEX", k, idx()]
    elif r < 51: cmd = [b"LSET", k, idx(), val()]
    elif r < 56: cmd = [b"LINSERT", k, rng.choice([b"BEFORE", b"AFTER", b"after"]), val(), val()]
    elif r < 63: cmd = [b"LREM", k, b"%d" % rng.randrange(-3, 4), val()]
    elif r < 66: cmd = [b"LTRIM", k, idx(), idx()]
    elif r < 72:
        cmd = [b"LPOS", k, val()]
        if rng.random() < .5: cmd += [b"RANK", b"%d" % rng.choice([-3, -2, -1, 1, 2, 3])]
        if rng.random() < .5: cmd += [b"COUNT", b"%d" % rng.randrange(0, 4)]
        if rng.random() < .3: cmd += [b"MAXLEN", b"%d" % rng.randrange(0, 50)]
    elif r < 77: cmd = [b"LMOVE", k, rng.choice(keys), side(), side()]
    elif r < 79: cmd = [b"RPOPLPUSH", k, rng.choice(keys)]
    elif r < 83:
        # bloquantes servies tout de suite (clé non vide) ou timeout très court
        kk = [k] + ([rng.choice(keys)] if rng.random() < .5 else [])
        cmd = [rng.choice([b"BLPOP", b"BRPOP"])] + kk + [b"0.01"]
    elif r < 85: cmd = [b"BLMOVE", k, rng.choice(keys), side(), side(), b"0.01"]
    elif r < 86: cmd = [b"BRPOPLPUSH", k, rng.choice(keys), b"0.01"]
    elif r < 88: cmd = [b"SET", b"str", b"x"] if rng.random() < .5 else [rng.choice([b"LPUSH", b"LRANGE", b"LPOP"]), b"str"] + ([b"a"] if rng.random() < .5 else [b"0", b"1"])
    elif r < 89: cmd = [b"DEL", k]
    elif r < 90: cmd = [b"TYPE", k]
    else: cmd = [b"LRANGE", k, b"0", b"-1"]
    # grandes listes : garder plusieurs blocs (ni LTRIM ni DEL)
    if len(sys.argv) > 5 and cmd[0] in (b"LTRIM", b"DEL"):
        cmd = [b"LRANGE", k, idx(), b"%d" % rng.randrange(1500, 2100)]
    a, b = send(rx, cmd), send(vk, cmd)
    ops += 1
    if a != b:
        mismatch += 1
        if mismatch <= 15:
            print("DIFF", b" ".join(cmd).decode(errors="replace"))
            print("  radixox:", a)
            print("  valkey :", b)
for k in keys:
    a = send(rx, [b"LRANGE", k, b"0", b"-1"]); b = send(vk, [b"LRANGE", k, b"0", b"-1"])
    if a != b:
        mismatch += 1; print("FINAL DIFF", k)
    print(k.decode(), "len", send(rx, [b"LLEN", k]))
print(f"{ops} commandes, {mismatch} différences")
