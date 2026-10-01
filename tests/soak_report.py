#!/usr/bin/env python3
"""Report of a soak run (tests/soak.sh): reads the samples and pgbench logs
in the run's output directory and prints, per time window, latency
percentiles per script, memory per backend (RSS and pg_automerge's
counters), the container's memory, table/TOAST/index sizes and bloat, WAL
and notifications, plus the growth rates the soak's checks use.

Usage: tests/soak_report.py OUT_DIR [WINDOWS]   (standard library only)
"""

import csv
import glob
import os
import statistics
import sys


def read_csv(path):
    if not os.path.exists(path):
        return []
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def num(v, default=None):
    try:
        return float(v)
    except (TypeError, ValueError):
        return default


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * p / 100.0
    lo = int(k)
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def slope_per_hour(points):
    """Least-squares slope of (t seconds, value) points, per hour."""
    if len(points) < 3:
        return None
    ts = [p[0] for p in points]
    vs = [p[1] for p in points]
    mt, mv = statistics.fmean(ts), statistics.fmean(vs)
    den = sum((t - mt) ** 2 for t in ts)
    if den == 0:
        return None
    return sum((t - mt) * (v - mv) for t, v in zip(ts, vs)) / den * 3600


def fmt(v, digits=1, width=9):
    if v is None:
        return "-".rjust(width)
    return f"{v:,.{digits}f}".rjust(width)


def table(title, header, rows):
    print(f"\n{title}")
    widths = [max(len(str(h)), *(len(str(r[i])) for r in rows)) if rows else len(str(h)) for i, h in enumerate(header)]
    print("  " + "  ".join(str(h).rjust(w) for h, w in zip(header, widths)))
    for r in rows:
        print("  " + "  ".join(str(c).rjust(w) for c, w in zip(r, widths)))


def main():
    out = sys.argv[1]
    params = {}
    with open(os.path.join(out, "params.txt")) as f:
        for line in f:
            k, _, v = line.rstrip("\n").partition("=")
            params[k] = v
    duration = float(params["duration_s"])
    scripts = {int(r["script_no"]): r["script"] for r in read_csv(os.path.join(out, "scripts.csv"))}

    # pgbench per-transaction logs: client tx time_us script epoch us [lag] [retries]
    tx = []  # (epoch seconds, script, latency ms or None when failed)
    for path in glob.glob(os.path.join(out, "pgbench_log", "pgbench*")):
        with open(path) as f:
            for line in f:
                p = line.split()
                if len(p) < 6:
                    continue
                t = int(p[4]) + int(p[5]) / 1e6
                lat = num(p[2])
                tx.append((t, int(p[3]), None if lat is None else lat / 1000.0))
    if not tx:
        print("no pgbench transactions logged")
        return 1
    t0 = min(t for t, _, _ in tx)
    t_end = max(t for t, _, _ in tx)
    span = max(t_end - t0, 1.0)
    windows = int(sys.argv[2]) if len(sys.argv) > 2 else (6 if duration >= 1800 else 3)
    wlen = span / windows

    def window_of(t):
        return min(max(int((t - t0) / wlen), 0), windows - 1)

    wlabels = [f"{int(i * wlen / 60)}-{int((i + 1) * wlen / 60)}m" if wlen >= 120 else f"{int(i * wlen)}-{int((i + 1) * wlen)}s" for i in range(windows)]

    print(f"Soak run {os.path.basename(out)}: {params.get('image')}, {params.get('clients')} clients, "
          f"{span / 60:.1f} min, documents up to {params.get('doc_size_kb')} kB, memory limit {params.get('memory')}, "
          f"max_load_memory {params.get('max_load_memory')}, shared_buffers {params.get('shared_buffers')}")
    print(f"weights: {params.get('weights')}")

    # ---- throughput and latency ------------------------------------------
    by = {}
    failed = {}
    for t, s, lat in tx:
        if lat is None:
            failed[s] = failed.get(s, 0) + 1
            continue
        by.setdefault((s, window_of(t)), []).append(lat)
    total = len(tx)
    clients = int(params.get("clients", 1))
    print(f"\ntransactions: {total:,} ({total / span:.1f}/s, {total / clients:,.0f} per client), failed: {sum(failed.values())}")
    rows = []
    for s in sorted(scripts):
        all_lat = [x for w in range(windows) for x in by.get((s, w), [])]
        if not all_lat:
            continue
        rows.append([scripts[s], f"{len(all_lat):,}", fmt(pct(all_lat, 50), 1, 1), fmt(pct(all_lat, 95), 1, 1),
                     fmt(pct(all_lat, 99), 1, 1), fmt(max(all_lat), 1, 1), failed.get(s, 0)])
    table("latency per script over the whole run (ms; a script is one transaction)",
          ["script", "count", "p50", "p95", "p99", "max", "failed"], rows)

    for p in (50, 95, 99):
        rows = []
        for s in sorted(scripts):
            vals = [pct(by.get((s, w), []), p) for w in range(windows)]
            if all(v is None for v in vals):
                continue
            first = next((v for v in vals if v is not None), None)
            last = vals[-1]
            drift = f"{last / first:.2f}x" if first and last else "-"
            rows.append([scripts[s]] + [fmt(v, 1, 1) for v in vals] + [drift])
        table(f"p{p} latency per window (ms)", ["script"] + wlabels + ["last/first"], rows)

    rows = []
    for s in sorted(scripts):
        rows.append([scripts[s]] + [f"{len(by.get((s, w), [])) / wlen:.2f}" for w in range(windows)])
    table("transactions per second per window", ["script"] + wlabels, rows)

    # ---- memory per backend ---------------------------------------------
    proc = read_csv(os.path.join(out, "proc.csv"))
    bench_pids = {r["pid"] for r in read_csv(os.path.join(out, "activity.csv"))}
    series = {}
    other = {}
    for r in proc:
        t = num(r["t"])
        if t is None:
            continue
        if r["pid"] in bench_pids:
            series.setdefault(r["pid"], []).append((t, num(r["anon_kb"], 0) / 1024, num(r["rss_kb"], 0) / 1024))
        else:
            title = r["title"].split("(")[0].strip()
            if title.startswith("postgres -c") or title == "postgres":
                title = "postmaster"
            if " app " in title or title.startswith("postgres: postgres"):
                title = "other client backends"
            other.setdefault(title, []).append(num(r["anon_kb"], 0) / 1024)
    warm = t0 + max(300.0, 0.1 * span)
    mid = t0 + 0.5 * span
    q3 = t0 + 0.75 * span
    rows = []
    growths = []
    highs = []
    for pid, pts in sorted(series.items()):
        pts = [p for p in pts if t0 - 60 <= p[0] <= t_end + 5]
        if not pts:
            continue
        after = [(t, a) for t, a, _ in pts if t >= warm]
        sl = slope_per_hour(after)
        second_q = [a for t, a, _ in pts if mid - 0.25 * span <= t < mid]
        last_q = [a for t, a, _ in pts if t >= q3]
        g = None
        if second_q and last_q:
            g = (statistics.median(last_q) - statistics.median(second_q)) / (q3 + 0.125 * span - (mid - 0.125 * span)) * 3600
            growths.append(g)
        anon = [a for _, a, _ in pts]
        first_half = [a for t, a, _ in pts if warm <= t < mid]
        second_half = [a for t, a, _ in pts if t >= mid]
        hw = None
        if first_half and second_half:
            hw = max(second_half) - max(first_half)
            highs.append(hw)
        rows.append([pid, fmt(anon[0]), fmt(statistics.median(second_q) if second_q else None),
                     fmt(statistics.median(last_q) if last_q else None), fmt(anon[-1]),
                     fmt(max(first_half) if first_half else None), fmt(max(second_half) if second_half else None),
                     fmt(max(r for _, _, r in pts)), fmt(sl), fmt(g)])
    table("pgbench backends: anonymous RSS (MB; the private memory: Rust heap, palloc, malloc's free lists)",
          ["pid", "first", "med 25-50%", "med 75-100%", "last", "max 1st half*", "max 2nd half",
           "max RSS", "slope/h**", "med growth/h***"], rows)
    print("  * after the warm-up (the first 5 minutes or 10% of the run); ** least-squares slope after the warm-up;"
          " *** median of the last quarter minus median of the second quarter, per hour")
    worst = max(highs) if highs else 0.0
    print(f"max backend anon RSS high-water growth: {worst:.1f} MB (max of the 2nd half minus max of the 1st)")
    if growths:
        print(f"median anon RSS growth rates: {min(growths):.1f} to {max(growths):.1f} MB/h")
    if other:
        table("other processes: anonymous RSS (MB)", ["process", "median", "max"],
              [[k, fmt(statistics.median(v)), fmt(max(v))] for k, v in sorted(other.items())])

    # ---- pg_automerge counters and memory contexts (self-sampled) -------
    mem = read_csv(os.path.join(out, "mem.csv"))
    if mem:
        rows = []
        for w in range(windows):
            ms = [m for m in mem if window_of(num(m["t"])) == w]
            if not ms:
                rows.append([wlabels[w]] + ["-"] * 6)
                continue
            rows.append([wlabels[w], len(ms), int(max(num(m["allocated_bytes"]) for m in ms)),
                         int(max(num(m["live_documents"]) for m in ms)),
                         fmt(max(num(m["peak_allocated_bytes"]) for m in ms) / 2**20),
                         fmt(statistics.median(num(m["contexts_total"]) for m in ms) / 2**20, 2),
                         fmt(max(num(m["contexts_total"]) for m in ms) / 2**20, 2)])
        table("automerge_memory_usage() and pg_backend_memory_contexts, sampled by the backends between statements",
              ["window", "samples", "max allocated B", "max live docs", "max peak MB", "med ctx MB", "max ctx MB"], rows)
        per_pid = {}
        for m in mem:
            per_pid.setdefault(m["pid"], []).append(m)
        rows = []
        for pid, ms in sorted(per_pid.items()):
            loads = num(ms[-1]["loads"])
            lt = num(ms[-1]["load_time"])
            ctx = [(num(m["t"]), num(m["contexts_total"]) / 2**20) for m in ms]
            ctx_after = [c for c in ctx if c[0] >= warm]
            rows.append([pid, len(ms), f"{int(loads):,}", fmt(lt / loads if loads else None, 2),
                         fmt(num(ms[-1]["peak_allocated_bytes"]) / 2**20),
                         fmt(ctx[0][1], 2), fmt(ctx[-1][1], 2), fmt(slope_per_hour(ctx_after), 2)])
        table("per backend (last sample)", ["pid", "samples", "loads", "ms/load", "peak MB", "ctx first MB", "ctx last MB", "ctx slope MB/h"], rows)

    # ---- container memory ---------------------------------------------------
    cg = read_csv(os.path.join(out, "cgroup.csv"))
    if cg:
        rows = []
        for w in range(windows):
            cs = [c for c in cg if window_of(num(c["t"])) == w]
            if cs:
                rows.append([wlabels[w], fmt(max(num(c["current"]) for c in cs) / 2**20, 0),
                             fmt(max(num(c["anon"]) for c in cs) / 2**20, 0),
                             fmt(statistics.median(num(c["anon"]) for c in cs) / 2**20, 0),
                             fmt(max(num(c["file"]) for c in cs) / 2**20, 0),
                             fmt(max(num(c["shmem"]) for c in cs) / 2**20, 0)])
        table("container memory (cgroup, MB)", ["window", "max current", "max anon", "med anon", "max file", "max shmem"], rows)

    # ---- tables, TOAST, bloat, WAL -------------------------------------------
    db = read_csv(os.path.join(out, "db.csv"))
    if db:
        rows = []
        prev_wal = num(db[0]["wal_bytes"])
        prev_t = num(db[0]["t"])
        picks = []
        for w in range(windows):
            ds = [d for d in db if window_of(num(d["t"])) == w and t0 <= num(d["t"]) <= t_end + 5]
            if ds:
                picks.append((wlabels[w], ds[-1]))
        picks.append(("after VACUUM", db[-1]))
        for label, d in picks:
            wal = num(d["wal_bytes"])
            t = num(d["t"])
            n = num(d["rows"]) or 1
            rows.append([label, int(num(d["rows"])), fmt(num(d["heap_bytes"]) / 2**20), fmt(num(d["toast_bytes"]) / 2**20),
                         fmt(num(d["index_bytes"]) / 2**20), fmt(num(d["total_bytes"]) / n / 1024),
                         int(num(d["dead_tup"])), d["heap_free_pct"], d["heap_dead_pct"],
                         int(num(d["toast_dead_tup"])), d["toast_free_pct"], d["toast_dead_pct"],
                         d["autovacuums"], d["toast_autovacuums"],
                         fmt((wal - prev_wal) / 2**20 / max((t - prev_t) / 60, 1e-9) if t > prev_t else None)])
            prev_wal, prev_t = wal, t
        table("soak_docs at the end of each window (sizes MB; free/dead: pgstattuple_approx %)",
              ["window", "rows", "heap", "toast", "indexes", "kB/row", "dead", "free%", "dead%",
               "toast dead", "t free%", "t dead%", "autovac", "t autovac", "WAL MB/min"], rows)
        rows = []
        for label, d in picks:
            rows.append([label, d["small_bytes"], d["medium_bytes"], d["big_bytes"], d["small_changes"],
                         d["medium_changes"], d["big_changes"], d["cursor_writes"], d["commits"], d["rollbacks"], d["deadlocks"]])
        table("documents: average stored size (bytes, compressed) and change count per class (small: slots 0-13, "
              "medium: 14-17, big: 18-19)",
              ["window", "small B", "medium B", "big B", "small ch", "medium ch", "big ch", "lane writes",
               "commits", "rollbacks", "deadlocks"], rows)
        first, last = db[0], db[-1]
        wal_total = (num(last["wal_bytes"]) - num(first["wal_bytes"])) / 2**20
        writes = (num(last["cursor_writes"]) or 0) - (num(first["cursor_writes"]) or 0)
        print(f"\nWAL: {wal_total:,.0f} MB over the run, {wal_total * 1024 / writes if writes else 0:,.1f} kB per lane write;"
              f" notification queue max {max(num(d['notify_queue'], 0) for d in db):.6f}")

    path = os.path.join(out, "listener.csv")
    if os.path.exists(path):
        with open(path) as f:
            last = None
            for line in f:
                last = line.strip().split(",")
        if last:
            print(f"notifications received: {int(last[1]):,} (malformed {last[2]})")
    vac = read_csv(os.path.join(out, "vacuum.csv"))
    if vac:
        print("manual VACUUM (ANALYZE) durations (ms): " + ", ".join(v["ms"] for v in vac))
    return 0


if __name__ == "__main__":
    sys.exit(main())
