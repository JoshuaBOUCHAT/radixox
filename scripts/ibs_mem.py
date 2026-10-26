#!/usr/bin/env python3
"""Classe les lectures échantillonnées par IBS (scripts/uuid_prof_l2.sh) par niveau
qui les sert, puis, pour un niveau (L2 par défaut), par symbole et par zone mémoire
(mapping de /proc/PID/maps). Les comptes sont ramenés à la requête :
échantillons × période / commandes exécutées pendant l'enregistrement.

  scripts/ibs_mem.py DIR            # DIR = $OUT/<mode> du bench
  scripts/ibs_mem.py DIR --lvl DRAM --top 30
  scripts/ibs_mem.py DIR --src exec_window --depth 3   # lieux d'inlining d'un symbole
  --pf : les préchargements logiciels (IBS les compte comme des lectures) au lieu des
  vraies lectures ; --thread loop|sqp|all : thread retenu (SQPOLL est un thread du
  processus, sur un autre cœur, donc une autre L2 ; défaut loop).
"""
import bisect, collections, gzip, re, sys

args = sys.argv[1:]
d = args[0]
lvl = args[args.index("--lvl") + 1] if "--lvl" in args else "L2"
top = int(args[args.index("--top") + 1]) if "--top" in args else 25

cmds, period = map(int, open(f"{d}/cmds").read().split())

maps = []
for line in open(f"{d}/maps"):
    f = line.split()
    a, b = (int(x, 16) for x in f[0].split("-"))
    name = f[5] if len(f) > 5 else "[anon]"
    maps.append((a, b, name, f[1]))
starts = [m[0] for m in maps]

def region(addr):
    i = bisect.bisect_right(starts, addr) - 1
    if i < 0 or addr >= maps[i][1]:
        return "?"
    a, b, name, perm = maps[i]
    size = b - a
    tag = name.rsplit("/", 1)[-1]
    if tag == "[anon]":
        unit = f"{size >> 20} Mio" if size >= 1 << 20 else f"{size >> 10} Kio"
        tag = f"anon {unit} @{a:x}"
    return tag

import subprocess
exe_path = next(m[2] for m in maps if m[3].startswith("r-x") and m[2].endswith("/radixox"))
load_base = next(m[0] for m in maps if m[2] == exe_path and m[3].startswith("r--"))
# adresses des prefetch*, mises en cache dans DIR (objdump du binaire entier est lent)
import os
cache = f"{d}/prefetch_ips"
if os.path.exists(cache):
    pf_ips = {int(x, 16) for x in open(cache).read().split()}
else:
    pf_ips = set()
    for l in subprocess.run(["objdump", "-d", "--no-show-raw-insn", exe_path], capture_output=True, text=True).stdout.splitlines():
        if "\tprefetch" in l:
            pf_ips.add(int(l.split(":")[0], 16))
    open(cache, "w").write("\n".join(hex(x) for x in sorted(pf_ips)))
pf = "--pf" in args  # --pf : ne garder que les préchargements ; sinon, que les vraies lectures

# thread : loop (boucle), sqp (SQPOLL, autre cœur donc autre L2) ou all
thr = args[args.index("--thread") + 1] if "--thread" in args else "loop"
def thread_ok(comm):
    return thr == "all" or (comm.startswith("iou-sqp")) == (thr == "sqp")
line_re = re.compile(r"^\s*(\S+)\s+([0-9a-f]+)\s+\S+\s+\|OP (\S+)\|LVL ([^|]+)\|.*?\s+([0-9a-f]+)\s+(\S+)\s*$")
by_lvl = collections.Counter()
by_sym = collections.Counter()
by_reg = collections.Counter()
by_sym_reg = collections.Counter()
lat = collections.defaultdict(list)
loads = 0
for raw in gzip.open(f"{d}/samples.gz", "rt", errors="replace"):
    m = line_re.match(raw)
    if not m:
        continue
    cpu, addr, op, level, ip, sym = m.groups()
    if op != "LOAD" or not thread_ok(cpu):
        continue
    is_pf = (int(ip, 16) - load_base) in pf_ips
    if is_pf != pf:
        continue
    loads += 1
    level = level.strip()
    # « L2 hit », « L1 hit », « RAM hit », « Remote Cache (1 hop) hit »…
    key = ("L1" if level.startswith("L1") else "L2" if level.startswith("L2")
           else "L3" if level.startswith("L3") else "DRAM" if "RAM" in level else level)
    by_lvl[key] += 1
    if key != lvl:
        continue
    reg = region(int(addr, 16))
    sym = re.sub(r"\+0x[0-9a-f]+$", "", sym)
    by_sym[sym] += 1
    by_reg[reg] += 1
    by_sym_reg[(sym, reg)] += 1
    w = raw.split()[1]

per = lambda n: n * period / cmds
print(f"{cmds} commandes, période {period} : {'préchargements' if pf else 'lectures (hors préchargements)'}/requête par niveau")
for k, n in by_lvl.most_common():
    print(f"  {k:>6} {per(n):7.2f}")
print(f"\n{lvl} : par zone")
for r, n in by_reg.most_common(top):
    print(f"  {per(n):6.2f}  {r}")
print(f"\n{lvl} : par symbole")
for s, n in by_sym.most_common(top):
    print(f"  {per(n):6.2f}  {s}")
print(f"\n{lvl} : par symbole × zone")
for (s, r), n in by_sym_reg.most_common(top):
    print(f"  {per(n):6.2f}  {s[:70]:70} {r}")

# --src SYM : pour ce symbole, chaînes d'inlining (addr2line -i) × zone
if "--src" in args:
    import subprocess
    want = args[args.index("--src") + 1]
    exe = next(m for m in maps if m[3].startswith("r-x") and m[2].endswith("/radixox"))
    # PIE : adresse virtuelle de l'ELF = ip − base de chargement (mapping d'offset 0 ;
    # le segment de code n'a pas vaddr == offset)
    for line in open(f"{d}/maps"):
        f = line.split()
        if len(f) > 5 and f[5] == exe[2] and int(f[2], 16) == 0:
            base = int(f[0].split("-")[0], 16)
    ips = collections.Counter()
    for raw in gzip.open(f"{d}/samples.gz", "rt", errors="replace"):
        m = line_re.match(raw)
        if not m or m.group(3) != "LOAD" or want not in m.group(6):
            continue
        if not thread_ok(m.group(1)):
            continue
        if ((int(m.group(5), 16) - load_base) in pf_ips) != pf:
            continue
        lv = m.group(4).strip()
        if not lv.startswith(lvl) and not (lvl == "DRAM" and "RAM" in lv):
            continue
        ips[(int(m.group(5), 16) - base, region(int(m.group(2), 16)))] += 1
    uniq = sorted({ip for ip, _ in ips})
    out = subprocess.run(["addr2line", "-e", exe[2], "-i", "-f", "-C", "-a"] + [hex(x) for x in uniq],
                         capture_output=True, text=True).stdout
    chains, cur = {}, None
    lines = out.splitlines()
    i = 0
    while i < len(lines):
        if lines[i].startswith("0x"):
            cur = int(lines[i], 16); chains[cur] = []; i += 1; continue
        fn, loc = lines[i], lines[i + 1] if i + 1 < len(lines) else "?"
        chains[cur].append((fn, "/".join(loc.split(" ")[0].split("/")[-2:])))
        i += 2
    depth = int(args[args.index("--depth") + 1]) if "--depth" in args else 1
    agg = collections.Counter()
    for (ip, reg), n in ips.items():
        ch = chains.get(ip, [("?", "?")])[:depth]
        agg[(" ← ".join(loc for _, loc in ch), reg)] += n
    print(f"\n{lvl} dans {want} : lieu (profondeur {depth}) × zone")
    for (loc, reg), n in agg.most_common(top):
        print(f"  {per(n):6.2f}  {loc[:90]:90} {reg}")
