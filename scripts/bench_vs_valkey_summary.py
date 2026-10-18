"""Synthèse de scripts/bench_vs_valkey.sh : médiane des rounds (min–max),
CPU serveur, YCSB. Usage : bench_vs_valkey_summary.py OUT ROUNDS TEST_TIME"""
import json, re, statistics, sys
from pathlib import Path

out, rounds, test_time = Path(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
NAMES = {"rx": "RadixOx", "vk": "Valkey"}


def memtier(srv, pipe, r):
    t = json.loads((out / f"run_{srv}_p{pipe}_r{r}.json").read_text())["ALL STATS"]["Totals"]
    p = t["Percentile Latencies"]
    cpu_ms = int((out / f"cpu_{srv}_p{pipe}_r{r}.txt").read_text())
    return {
        "ops": t["Ops/sec"],
        "p50": p["p50.00"], "p99": p["p99.00"], "p999": p["p99.90"], "p9999": p["p99.99"],
        # part d'un cœur occupée par le serveur, et opérations par seconde de CPU serveur
        "core": cpu_ms / 1000 / test_time,
        "ops_per_cpu_s": t["Ops/sec"] * test_time / (cpu_ms / 1000) if cpu_ms else float("nan"),
    }


def med(vals):
    return statistics.median(vals), min(vals), max(vals)


print(f"memtier : 1:4 SET:GET, {rounds} rounds de {test_time:.0f} s, médiane (min–max)\n")
for pipe in (1, 32):
    print(f"## pipeline {pipe}")
    print(f"{'':8} {'ops/s':>28} {'p50 ms':>8} {'p99 ms':>8} {'p99.9':>8} {'p99.99':>8} {'cœur':>6} {'ops/s CPU':>11}")
    for srv in ("rx", "vk"):
        runs = [memtier(srv, pipe, r) for r in range(1, rounds + 1)]
        o = med([x["ops"] for x in runs])
        cols = [med([x[k] for x in runs])[0] for k in ("p50", "p99", "p999", "p9999")]
        core = med([x["core"] for x in runs])[0]
        eff = med([x["ops_per_cpu_s"] for x in runs])[0]
        print(f"{NAMES[srv]:8} {o[0]:>10,.0f} ({o[1]:,.0f}–{o[2]:,.0f})"
              f" {cols[0]:8.3f} {cols[1]:8.3f} {cols[2]:8.3f} {cols[3]:8.3f} {core:6.2f} {eff:>11,.0f}")
    print()


def ycsb(path):
    txt = path.read_text()
    def stat(section, name):
        m = re.search(rf"\[{section}\], {re.escape(name)}, ([\d.]+)", txt)
        return float(m.group(1)) if m else float("nan")
    return stat, txt


print("## YCSB workload A (1 champ de 100 o, 100 threads, 1 run)")
print(f"{'':8} {'load ops/s':>11} {'run ops/s':>10} {'READ avg':>9} {'p50':>6} {'p99':>6} {'p99.9':>6} {'p99.99':>7} {'UPD p99':>8} {'RSS max':>8} {'cœur':>5}")
for srv in ("rx", "vk"):
    load, _ = ycsb(out / f"ycsb_load_{srv}.txt")
    run, _ = ycsb(out / f"ycsb_run_{srv}.txt")
    run_s = run("OVERALL", "RunTime(ms)") / 1000
    cpu_s = int((out / f"ycsb_cpu_{srv}.txt").read_text()) / 1000
    rss = (out / f"ycsb_rss_{srv}.txt").read_text()
    print(f"{NAMES[srv]:8} {load('OVERALL', 'Throughput(ops/sec)'):>11,.0f} {run('OVERALL', 'Throughput(ops/sec)'):>10,.0f}"
          f" {run('READ', 'AverageLatency(us)'):>9,.0f} {run('READ', '50thPercentileLatency(us)'):>6,.0f}"
          f" {run('READ', '99thPercentileLatency(us)'):>6,.0f} {run('READ', '99.9PercentileLatency(us)'):>6,.0f}"
          f" {run('READ', '99.99PercentileLatency(us)'):>7,.0f} {run('UPDATE', '99thPercentileLatency(us)'):>8,.0f}"
          f" {rss:>5} MB {cpu_s / run_s:5.2f}")
