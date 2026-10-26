#!/usr/bin/env python3
# heap_sites.py LOG : agrège les piles de RADIXOX_HEAP_TRACE (build --cfg radixox_heapcount avec
# symboles, MEMORY_ARCH §18.10) par les trois premiers lieux du dépôt : octets, nombre, taille max.
import collections, re, sys
txt = open(sys.argv[1], errors="replace").read()
agg = collections.defaultdict(lambda: [0, 0, 0])
for blk in txt.split("[heap] ")[1:]:
    m = re.match(r"(\d+) o", blk)
    if not m:
        continue
    size = int(m.group(1))
    locs = re.findall(r"at \./(\S+?:\d+)", blk)
    locs = [l for l in locs if "raw_alloc.rs" not in l and "bin/uringloop.rs:16" not in l]
    key = " ← ".join(locs[:3]) or "?"
    a = agg[key]; a[0] += 1; a[1] += size; a[2] = max(a[2], size)
for k, (n, b, mx) in sorted(agg.items(), key=lambda kv: -kv[1][1])[:25]:
    print(f"{b/1048576:8.2f} Mo  {n:6d} allocs  max {mx/1024:8.0f} Kio  {k}")
