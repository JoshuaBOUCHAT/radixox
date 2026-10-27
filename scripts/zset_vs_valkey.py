"""Différentiel ZSet : mêmes commandes à RadixOx et à Valkey, réponses comparées.

Usage : python3 -I scripts/zset_vs_valkey.py <port radixox> <port valkey> [graine] [commandes] [big]
(`big` : chaque clé part de 600 membres, grande forme). Base vidée au départ (FLUSHDB).
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
            return None
        d = f.read(n + 2)[:-2]
        return d
    if t == b"*":
        n = int(rest)
        return None if n < 0 else [read(f) for _ in range(n)]
    raise ValueError(line)

def norm(r):
    # scores : comparés en flottant (formatage différent des grands nombres)
    if isinstance(r, list):
        return [norm(x) for x in r]
    if isinstance(r, bytes):
        try:
            return ("f", float(r))
        except ValueError:
            return r
    return r

rx, vk = conn(int(sys.argv[1])), conn(int(sys.argv[2]))
rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 1)
for c in (rx, vk):
    send(c, [b"FLUSHDB"])
keys = [b"za", b"zb", b"zbig"]
def member():
    return b"m%d" % rng.randrange(400 if rng.random() < 0.5 else 20)
def score():
    return rng.choice([b"%d" % rng.randrange(-20, 20), b"%d.5" % rng.randrange(-5, 5), b"-inf", b"+inf", b"0"])
def sbound():
    v = rng.choice([b"%d" % rng.randrange(-22, 22), b"-inf", b"+inf", b"%d.5" % rng.randrange(-5, 5)])
    return (b"(" + v) if rng.random() < 0.3 and v[0:1] not in b"+-" or (rng.random() < 0.1) else v
def lbound():
    return rng.choice([b"-", b"+", b"[" + member(), b"(" + member()])
def lim():
    return [b"LIMIT", b"%d" % rng.randrange(0, 10), b"%d" % rng.randrange(-2, 15)]
# grande forme : chaque clé part de 600 membres (scores à ex-aequo)
if len(sys.argv) > 5:
    for k in keys + [b"zlex"]:
        args = [b"ZADD", k]
        for i in range(600):
            args += [b"0" if k == b"zlex" else b"%d" % (i % 37 - 18), b"m%d" % i]
        for c in (rx, vk):
            send(c, args)
for c in (rx, vk):
    send(c, [b"SADD", b"sset"] + [b"m%d" % i for i in range(0, 400, 7)])
    send(c, [b"SET", b"str", b"x"])
ops = 0
mismatch = 0
for step in range(int(sys.argv[4]) if len(sys.argv) > 4 else 20000):
    k = rng.choice(keys)
    r = rng.randrange(100)
    if r < 30:
        n = rng.randrange(1, 6)
        flags = [f for f in (b"NX", b"XX", b"GT", b"LT", b"CH") if rng.random() < 0.1]
        cmd = [b"ZADD", k] + flags
        if rng.random() < 0.1:
            cmd += [b"INCR", score(), member()]
        else:
            for _ in range(n):
                cmd += [score(), member()]
    elif r < 38: cmd = [b"ZREM", k] + [member() for _ in range(rng.randrange(1, 4))]
    elif r < 42: cmd = [b"ZINCRBY", k, score(), member()]
    elif r < 46: cmd = [b"ZSCORE", k, member()]
    elif r < 48: cmd = [b"ZMSCORE", k, member(), member(), member()]
    elif r < 50: cmd = [b"ZCARD", k]
    elif r < 56: cmd = [b"ZRANGE", k, b"%d" % rng.randrange(-30, 30), b"%d" % rng.randrange(-30, 30)] + ([b"REV"] if rng.random() < .5 else []) + ([b"WITHSCORES"] if rng.random() < .5 else [])
    elif r < 60:
        rev = rng.random() < .5
        cmd = [b"ZRANGE", k, sbound(), sbound(), b"BYSCORE"] + ([b"REV"] if rev else []) + (lim() if rng.random() < .5 else []) + ([b"WITHSCORES"] if rng.random() < .5 else [])
    elif r < 62: cmd = [b"ZRANGEBYSCORE", k, sbound(), sbound()] + ([b"WITHSCORES"] if rng.random() < .5 else []) + (lim() if rng.random() < .5 else [])
    elif r < 64: cmd = [b"ZREVRANGEBYSCORE", k, sbound(), sbound()] + ([b"WITHSCORES"] if rng.random() < .5 else []) + (lim() if rng.random() < .5 else [])
    elif r < 66: cmd = [b"ZREVRANGE", k, b"%d" % rng.randrange(-30, 30), b"%d" % rng.randrange(-30, 30), b"WITHSCORES"]
    elif r < 69: cmd = [b"ZRANK", k, member()] + ([b"WITHSCORE"] if rng.random() < .5 else [])
    elif r < 71: cmd = [b"ZREVRANK", k, member()] + ([b"WITHSCORE"] if rng.random() < .5 else [])
    elif r < 74: cmd = [b"ZCOUNT", k, sbound(), sbound()]
    elif r < 76: cmd = [b"ZPOPMIN" if rng.random() < .5 else b"ZPOPMAX", k] + ([b"%d" % rng.randrange(0, 5)] if rng.random() < .5 else [])
    elif r < 78: cmd = [b"ZREMRANGEBYRANK", k, b"%d" % rng.randrange(-10, 10), b"%d" % rng.randrange(-10, 10)]
    elif r < 80: cmd = [b"ZREMRANGEBYSCORE", k, sbound(), sbound()]
    # lex : seulement sur un ZSet à scores égaux (sens défini)
    elif r < 86:
        lk = b"zlex"
        sub = rng.randrange(6)
        if sub < 2: cmd = [b"ZADD", lk, b"0", member(), b"0", member()]
        elif sub == 2: cmd = [b"ZRANGEBYLEX", lk, lbound(), lbound()] + (lim() if rng.random() < .5 else [])
        elif sub == 3: cmd = [b"ZREVRANGEBYLEX", lk, lbound(), lbound()] + (lim() if rng.random() < .5 else [])
        elif sub == 4: cmd = [b"ZLEXCOUNT", lk, lbound(), lbound()]
        else: cmd = [b"ZREMRANGEBYLEX", lk, lbound(), lbound()] if rng.random() < .2 else [b"ZRANGE", lk, lbound(), lbound(), b"BYLEX"] + ([b"REV"] if rng.random() < .5 else [])
    elif r < 88:
        # ZRANDMEMBER : seulement la forme (taille, appartenance)
        n = rng.randrange(-5, 8)
        a = send(rx, [b"ZRANDMEMBER", k, b"%d" % n]); b = send(vk, [b"ZRANDMEMBER", k, b"%d" % n])
        full = send(vk, [b"ZRANGE", k, b"0", b"-1"])
        ok = isinstance(a, list) and len(a) == len(b) and all(x in full for x in a) and (n < 0 or len(set(a)) == len(a))
        if not ok:
            print("ZRANDMEMBER", k, n, a, b); mismatch += 1
        continue
    elif r < 89: cmd = [b"DEL", k]
    elif r < 95:
        # ZUNION/ZINTER/ZDIFF (+STORE), entrées : ZSet, Set (score 1), absente, chaîne (WRONGTYPE rare)
        src = keys + [b"zlex", b"sset", b"nokey"] + ([b"str"] if rng.random() < .05 else [])
        ins = [rng.choice(src) for _ in range(rng.randrange(1, 4))]
        op = rng.choice([b"ZUNION", b"ZINTER", b"ZDIFF"])
        store = rng.random() < .4
        cmd = ([op + b"STORE", rng.choice(keys + [b"zdst"])] if store else [op]) + [b"%d" % len(ins)] + ins
        if op != b"ZDIFF":
            if rng.random() < .4:
                cmd += [b"WEIGHTS"] + [rng.choice([b"1", b"2", b"-1", b"0.5", b"0", b"inf"]) for _ in ins]
            if rng.random() < .4:
                cmd += [b"AGGREGATE", rng.choice([b"SUM", b"MIN", b"MAX"])]
        if not store and rng.random() < .5:
            cmd += [b"WITHSCORES"]
    elif r < 96:
        ins = [rng.choice(keys + [b"sset", b"nokey"]) for _ in range(rng.randrange(1, 4))]
        cmd = [b"ZINTERCARD", b"%d" % len(ins)] + ins + ([b"LIMIT", b"%d" % rng.randrange(0, 20)] if rng.random() < .5 else [])
    elif r < 98:
        # ZSCAN : parcours complet de chaque côté, comparé comme ensemble
        pat = rng.choice([None, b"m1*", b"m?"])
        nos = rng.random() < .3
        res = []
        for c in (rx, vk):
            cur, got, n = b"0", {}, 0
            while True:
                q = [b"ZSCAN", k, cur] + ([b"MATCH", pat] if pat else []) + [b"COUNT", b"%d" % rng.randrange(1, 30)] + ([b"NOSCORES"] if nos else [])
                cur, items = send(c, q)
                step_ = 1 if nos else 2
                for i in range(0, len(items), step_):
                    got[items[i]] = None if nos else float(items[i + 1])
                n += 1
                if cur == b"0" or n > 10000:
                    break
            res.append(got)
        if res[0] != res[1]:
            mismatch += 1; print("ZSCAN", k, pat, nos, len(res[0]), len(res[1]))
        continue
    else: cmd = [b"ZRANGE", k, b"0", b"-1", b"WITHSCORES"]
    a, b = send(rx, cmd), send(vk, cmd)
    ops += 1
    if cmd[0].endswith(b"STORE") and norm(a) == norm(b):
        q = [b"ZRANGE", cmd[1], b"0", b"-1", b"WITHSCORES"]
        a, b = send(rx, q), send(vk, q)
        cmd = cmd + [b"=>"] + q
    if norm(a) != norm(b):
        mismatch += 1
        if mismatch <= 15:
            print("DIFF", b" ".join(cmd).decode(errors="replace"))
            print("  radixox:", a)
            print("  valkey :", b)
for k in keys + [b"zlex", b"zdst"]:
    a = send(rx, [b"ZRANGE", k, b"0", b"-1", b"WITHSCORES"]); b = send(vk, [b"ZRANGE", k, b"0", b"-1", b"WITHSCORES"])
    if norm(a) != norm(b):
        mismatch += 1; print("FINAL DIFF", k)
    print(k.decode(), "card", send(rx, [b"ZCARD", k]))
print(f"{ops} commandes, {mismatch} différences")
