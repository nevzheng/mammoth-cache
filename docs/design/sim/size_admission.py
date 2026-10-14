#!/usr/bin/env python3
"""Trace-driven model of a byte-bounded cache under different size limits and admission policies.

Used by design doc 0007. The workload is synthetic (see `make_trace`); the numbers it prints are
modelled, not measured. Requires numpy. Run: `python3 docs/design/sim/size_admission.py`.
"""

from collections import OrderedDict
import argparse

import numpy as np

KiB, MiB, GiB = 1 << 10, 1 << 20, 1 << 30


def make_trace(rng, keys, requests, zipf_a, median, sigma, big_frac, big_lo, big_hi):
    """Independent-reference trace: Zipf popularity, lognormal sizes plus a tail of big entries.

    Size and popularity are drawn independently. `big_frac` of keys are "distribution-like"
    entries, log-uniform between big_lo and big_hi.
    """
    sizes = rng.lognormal(np.log(median), sigma, keys)
    big = rng.random(keys) < big_frac
    sizes[big] = np.exp(rng.uniform(np.log(big_lo), np.log(big_hi), big.sum()))
    sizes = np.maximum(sizes, 64).astype(np.int64)
    ranks = np.arange(1, keys + 1, dtype=np.float64)
    p = ranks**-zipf_a
    p /= p.sum()
    trace = rng.choice(keys, size=requests, p=p)
    return sizes, trace


class Lru:
    """LRU bounded by bytes. `limit` drops entries larger than it (accept-and-drop)."""

    def __init__(self, capacity, limit):
        self.capacity, self.limit, self.used = capacity, limit, 0
        self.items = OrderedDict()

    def access(self, key, size):
        if key in self.items:
            self.items.move_to_end(key)
            return True
        if size > self.limit or size > self.capacity:
            return False
        while self.used + size > self.capacity:
            _, s = self.items.popitem(last=False)
            self.used -= s
        self.items[key] = size
        self.used += size
        return False


class FreqAdmitLru(Lru):
    """LRU with TinyLFU-style "aggregated victims" admission (Einziger et al., 2021).

    A new entry is admitted only if its recent frequency exceeds the summed frequency of the
    entries it would evict. Frequencies are exact counts halved every `window` accesses, standing
    in for TinyLFU's count-min sketch with aging.
    """

    def __init__(self, capacity, limit, window):
        super().__init__(capacity, limit)
        self.freq, self.window, self.ticks = {}, window, 0

    def _touch(self, key):
        self.freq[key] = self.freq.get(key, 0) + 1
        self.ticks += 1
        if self.ticks >= self.window:
            self.ticks = 0
            self.freq = {k: v >> 1 for k, v in self.freq.items() if v > 1}

    def access(self, key, size):
        self._touch(key)
        if key in self.items:
            self.items.move_to_end(key)
            return True
        if size > self.limit or size > self.capacity:
            return False
        need = self.used + size - self.capacity
        if need > 0:
            cand, victims, freed, vf = self.freq.get(key, 0), [], 0, 0
            for k, s in self.items.items():  # oldest first
                victims.append((k, s))
                freed += s
                vf += self.freq.get(k, 0)
                if vf >= cand:  # early pruning: candidate already lost
                    return False
                if freed >= need:
                    break
            for k, s in victims:
                del self.items[k]
                self.used -= s
        self.items[key] = size
        self.used += size
        return False


def make_savings(rng, sizes, compute_median_s, rebuild_mib_s, download_mib_s, rtt_s):
    """Seconds a hit saves: rebuild time minus download time, floored at zero.

    Rebuild time is a lognormal compute part plus writing the output at `rebuild_mib_s`
    (compression and disk). Download time is the round trip plus the bytes at `download_mib_s`.
    """
    compute = rng.lognormal(np.log(compute_median_s), 1.0, sizes.size)
    rebuild = compute + sizes / (rebuild_mib_s * MiB)
    download = rtt_s + sizes / (download_mib_s * MiB)
    return np.maximum(rebuild - download, 0.0)


def run(cache, sizes, savings, trace, warmup):
    """Returns hit ratio, byte hit ratio and time-saved ratio over the post-warmup requests.

    The time-saved ratio is seconds saved over the seconds an infinite cache would save.
    """
    hits = bytes_hit = reqs = bytes_req = saved = best = 0
    for i, k in enumerate(trace):
        s = int(sizes[k])
        h = cache.access(k, s)
        if i >= warmup:
            reqs += 1
            bytes_req += s
            best += savings[k]
            if h:
                hits += 1
                bytes_hit += s
                saved += savings[k]
    return hits / reqs, bytes_hit / bytes_req, saved / best


def fmt(n):
    for unit, v in (("GiB", GiB), ("MiB", MiB), ("KiB", KiB)):
        if n >= v:
            return f"{n / v:g} {unit}"
    return f"{n} B"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keys", type=int, default=200_000)
    ap.add_argument("--requests", type=int, default=2_000_000)
    ap.add_argument("--zipf", type=float, default=0.9)
    ap.add_argument("--median-kib", type=float, default=16)
    ap.add_argument("--sigma", type=float, default=2.0)
    ap.add_argument("--big-frac", type=float, default=0.002)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--capacities-gib", default="2,8,32")
    ap.add_argument("--compute-median-s", type=float, default=2.0)
    ap.add_argument("--rebuild-mib-s", type=float, default=150.0)
    ap.add_argument("--download-mib-s", type=float, default=110.0)  # about 1 Gbit/s
    ap.add_argument("--rtt-ms", type=float, default=5.0)
    args = ap.parse_args()

    rng = np.random.default_rng(args.seed)
    sizes, trace = make_trace(
        rng, args.keys, args.requests, args.zipf, args.median_kib * KiB, args.sigma,
        args.big_frac, 100 * MiB, 4 * GiB,
    )
    savings = make_savings(rng, sizes, args.compute_median_s, args.rebuild_mib_s,
                           args.download_mib_s, args.rtt_ms / 1000)
    total = int(sizes.sum())
    over = sizes > 100 * MiB
    print(f"keys={args.keys} requests={args.requests} zipf={args.zipf} "
          f"median={args.median_kib}KiB sigma={args.sigma} big_frac={args.big_frac}")
    print(f"working set {fmt(total)}; keys over 100 MiB: {over.mean():.2%} of keys, "
          f"{sizes[over].sum() / total:.1%} of bytes")
    for q in (50, 90, 99, 99.9):
        print(f"  p{q} size {fmt(int(np.percentile(sizes, q)))}")
    warmup = args.requests // 4
    limits = [("none", 1 << 62), ("1 GiB", GiB), ("100 MiB", 100 * MiB), ("10 MiB", 10 * MiB),
              ("1 MiB", MiB)]
    print("\n| capacity | policy | limit | hit ratio | byte hit ratio | time saved |")
    print("| --- | --- | --- | --- | --- | --- |")
    for cap in (int(float(c) * GiB) for c in args.capacities_gib.split(",")):
        for name, lim in limits:
            h, b, t = run(Lru(cap, lim), sizes, savings, trace, warmup)
            print(f"| {fmt(cap)} | LRU | {name} | {h:.1%} | {b:.1%} | {t:.1%} |")
        for name, lim in (limits[0], limits[2]):
            h, b, t = run(FreqAdmitLru(cap, lim, 10 * args.keys), sizes, savings, trace, warmup)
            print(f"| {fmt(cap)} | LRU + frequency admission | {name} | {h:.1%} | {b:.1%} "
                  f"| {t:.1%} |")


if __name__ == "__main__":
    main()
