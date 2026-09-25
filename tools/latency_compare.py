#!/usr/bin/env python3
"""End-to-end HTTP latency comparison of two navpath-service builds.

Starts each build on its own port with the same environment, drives the same request
set through it, and writes a markdown comparison table (plus a JSON dump of the raw
numbers). Both builds are measured the same way, one after the other; rounds alternate
A/B so slow drifts on the host (other processes, page cache) hit both sides equally.

Measured per build (median over rounds):
  * startup: process spawn -> /health 200 (ready gate);
  * search latency with the route cache OFF (NAVPATH_ROUTE_CACHE=0): client wall time
    and the server's own duration_us, per request class (golden corpus, random
    unseeded, random seeded), p50 / p90 / p99 / mean / max;
  * cache-hit latency (cache ON, second pass over the same requests);
  * concurrency: N client threads over the random set, cache OFF — throughput, p50,
    p99 and the number of 503 (admission) rejections;
  * resident memory (VmRSS) and swap after the run.
It also checks that both builds return the same found/cost for every request (costs
are optimal, so they must agree; paths may differ between equal-cost alternatives).

Usage (see --help):
  tools/latency_compare.py --base-bin OLD/navpath-service --base-snapshot old.snapshot \
                           --new-bin NEW/navpath-service --new-snapshot new.snapshot \
                           --out docs/latency_comparison.md
"""

import argparse
import concurrent.futures as cf
import json
import os
import signal
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

RICH_PROFILE = {"requirements": [{"key": "coins", "value": 100000000}, {"key": "hasDungCape", "value": 1}]}


def post(port, body, timeout=60):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/route",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = r.read()
            status = r.status
    except urllib.error.HTTPError as e:
        payload = e.read()
        status = e.code
    dt = (time.perf_counter() - t0) * 1e3
    data = None
    if status == 200:
        try:
            data = json.loads(payload)
        except ValueError:
            data = None
    return status, dt, data


def health(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except OSError:
        return None


def build_requests(pairs_file, n_random, n_seeded):
    reqs = []
    corpus = json.load(open(os.path.join(ROOT, "tools", "golden_corpus.json")))["entries"]
    for e in corpus:
        body = {
            "start": {"wx": e["start"][0], "wy": e["start"][1], "plane": e["start"][2]},
            "goal": {"wx": e["goal"][0], "wy": e["goal"][1], "plane": e["goal"][2]},
            "options": {"return_geometry": True, "only_actions": True},
            "surge": {"enabled": True, "charges": 2, "cooldown_ms": 20400},
            "dive": {"enabled": True, "cooldown_ms": 20400},
        }
        reqs_ = []
        if e.get("profile") == "all":
            reqs_ = list(RICH_PROFILE["requirements"])
        if e.get("quick_tele"):
            reqs_.append({"key": "hasQuickTele", "value": 1})
        if reqs_:
            body["profile"] = {"requirements": reqs_}
        reqs.append(("golden", body))
    pairs = []
    for line in open(pairs_file):
        v = [int(t) for t in line.split()]
        if len(v) == 6:
            pairs.append(v)
    for i, p in enumerate(pairs[: n_random + n_seeded]):
        body = {
            "start": {"wx": p[0], "wy": p[1], "plane": p[2]},
            "goal": {"wx": p[3], "wy": p[4], "plane": p[5]},
            "profile": RICH_PROFILE,
            "options": {"return_geometry": True, "only_actions": True},
            "surge": {"enabled": True, "charges": 2, "cooldown_ms": 20400},
            "dive": {"enabled": True, "cooldown_ms": 20400},
        }
        if i >= n_random:
            body["seed"] = 1000 + i
            reqs.append(("random_seeded", body))
        else:
            reqs.append(("random", body))
    return reqs


def pct(xs, q):
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = min(len(xs) - 1, max(0, int(round(q / 100.0 * (len(xs) - 1)))))
    return xs[k]


def summarize(xs):
    return {
        "n": len(xs),
        "p50": pct(xs, 50),
        "p90": pct(xs, 90),
        "p99": pct(xs, 99),
        "mean": statistics.fmean(xs) if xs else float("nan"),
        "max": max(xs) if xs else float("nan"),
    }


def proc_mem(pid):
    out = {}
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith(("VmRSS:", "VmSwap:")):
                k, v = line.split(":", 1)
                out[k] = int(v.split()[0]) / 1024.0  # MiB
    except OSError:
        pass
    return out


class Server:
    def __init__(self, label, binary, snapshot, port, env_extra, cache_on, log_dir):
        self.label, self.binary, self.snapshot, self.port = label, binary, snapshot, port
        env = dict(os.environ)
        env.update(env_extra)
        env["SNAPSHOT_PATH"] = snapshot
        env["NAVPATH_HOST"] = "127.0.0.1"
        env["NAVPATH_PORT"] = str(port)
        env["NAVPATH_ROUTE_CACHE"] = "2048" if cache_on else "0"
        env.setdefault("RUST_LOG", "warn")
        self.env = env
        self.log = open(os.path.join(log_dir, f"{label}_{port}_{'cache' if cache_on else 'nocache'}.log"), "w")
        self.proc = None

    def start(self):
        t0 = time.perf_counter()
        self.proc = subprocess.Popen([self.binary], env=self.env, stdout=self.log, stderr=subprocess.STDOUT)
        while True:
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.label} exited during startup (see log)")
            if health(self.port) == 200:
                return (time.perf_counter() - t0) * 1e3
            if time.perf_counter() - t0 > 300:
                raise RuntimeError(f"{self.label} not ready after 300 s")
            time.sleep(0.02)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def run_build(label, binary, snapshot, port, env_extra, reqs, concurrency, log_dir):
    res = {}
    # --- cache OFF: search latency + concurrency ---
    srv = Server(label, binary, snapshot, port, env_extra, False, log_dir)
    try:
        res["startup_ms"] = srv.start()
        for _, body in reqs[:20]:  # warm-up (thread pool, first-touch)
            post(port, body)
        per_class, per_class_srv, answers = {}, {}, []
        for cls, body in reqs:
            status, dt, data = post(port, body)
            per_class.setdefault(cls, []).append(dt)
            if data is not None:
                per_class_srv.setdefault(cls, []).append(data.get("duration_us", 0) / 1e3)
                answers.append((data.get("found"), data.get("cost")))
            else:
                answers.append((None, status))
        res["latency"] = {c: summarize(v) for c, v in per_class.items()}
        res["latency"]["all"] = summarize([x for v in per_class.values() for x in v])
        res["server_ms"] = {c: summarize(v) for c, v in per_class_srv.items()}
        res["answers"] = answers
        # Concurrency over the random requests.
        rnd = [b for c, b in reqs if c != "golden"]
        jobs = (rnd * ((4 * concurrency * 25) // max(1, len(rnd)) + 1))[: concurrency * 25]
        lat, rejects, errors = [], 0, 0
        t0 = time.perf_counter()
        with cf.ThreadPoolExecutor(max_workers=concurrency) as ex:
            for status, dt, _ in ex.map(lambda b: post(port, b), jobs):
                if status == 200:
                    lat.append(dt)
                elif status == 503:
                    rejects += 1
                else:
                    errors += 1
        wall = time.perf_counter() - t0
        res["concurrency"] = {
            "clients": concurrency,
            "requests": len(jobs),
            "throughput_rps": len(jobs) / wall,
            "p50": pct(lat, 50),
            "p99": pct(lat, 99),
            "rejects_503": rejects,
            "errors": errors,
        }
        res["mem_nocache"] = proc_mem(srv.proc.pid)
    finally:
        srv.stop()
    # --- cache ON: hit latency ---
    srv = Server(label, binary, snapshot, port, env_extra, True, log_dir)
    try:
        srv.start()
        for _, body in reqs:
            post(port, body)
        hits = [post(port, body)[1] for _, body in reqs]
        res["cache_hit"] = summarize(hits)
        res["mem_cache"] = proc_mem(srv.proc.pid)
    finally:
        srv.stop()
    return res


def med(runs, getter):
    vals = [getter(r) for r in runs]
    vals = [v for v in vals if v is not None and v == v]
    return statistics.median(vals) if vals else float("nan")


def change(a, b, kind="time"):
    """Change column. time: lower is better ("x faster/slower"); rate: higher is better
    ("x"); mem: relative size change; count: absolute difference."""
    if not (a == a and b == b):
        return "n/a"
    if kind == "count":
        return "same" if a == b else f"{b - a:+.0f}"
    if a == 0 or b == 0:
        return "n/a"
    if kind == "mem":
        return f"{(b - a) / a * 100:+.0f}%"
    if kind == "rate":
        return f"{b / a:.2f}x"
    r = a / b
    return f"{r:.2f}x {'faster' if r >= 1 else 'slower'}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base-bin", required=True)
    ap.add_argument("--base-snapshot", required=True)
    ap.add_argument("--new-bin", required=True)
    ap.add_argument("--new-snapshot", required=True)
    ap.add_argument("--pairs-file", default=os.path.join(ROOT, "target", "tmp", "engine_oracle_pairs.txt"))
    ap.add_argument("--random", type=int, default=150, help="unseeded random requests")
    ap.add_argument("--seeded", type=int, default=50, help="seeded random requests")
    ap.add_argument("--concurrency", type=int, default=16)
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--port", type=int, default=18301)
    ap.add_argument("--env", action="append", default=[], help="K=V applied to both builds (repeatable)")
    ap.add_argument("--out", default=os.path.join(ROOT, "docs", "latency_comparison.md"))
    args = ap.parse_args()

    env_extra = {"NAVPATH_JPS": "1", "NAVPATH_RACE": "1"}
    for kv in args.env:
        k, v = kv.split("=", 1)
        env_extra[k] = v
    reqs = build_requests(args.pairs_file, args.random, args.seeded)
    log_dir = os.path.join(ROOT, "target", "tmp", "latency_logs")
    os.makedirs(log_dir, exist_ok=True)

    runs = {"base": [], "new": []}
    for r in range(args.rounds):
        order = [("base", args.base_bin, args.base_snapshot), ("new", args.new_bin, args.new_snapshot)]
        if r % 2 == 1:
            order.reverse()
        for label, binary, snap in order:
            print(f"round {r + 1}: {label} ...", file=sys.stderr, flush=True)
            runs[label].append(run_build(label, binary, snap, args.port, env_extra, reqs, args.concurrency, log_dir))

    # Answer agreement (first round of each).
    mism = 0
    for (fa, ca), (fb, cb) in zip(runs["base"][0]["answers"], runs["new"][0]["answers"]):
        if fa != fb or (fa and isinstance(ca, (int, float)) and isinstance(cb, (int, float)) and abs(ca - cb) > 1e-3 * max(1.0, abs(ca))):
            mism += 1

    B, N = runs["base"], runs["new"]
    rows = []

    def row(name, get, unit="ms", kind="time", fmt="{:.2f}"):
        a, b = med(B, get), med(N, get)
        rows.append((name, (fmt.format(a) + f" {unit}").strip(), (fmt.format(b) + f" {unit}").strip(), change(a, b, kind)))

    sa, sb = os.path.getsize(args.base_snapshot) / 2**20, os.path.getsize(args.new_snapshot) / 2**20
    rows.append(("Snapshot file size", f"{sa:.0f} MiB", f"{sb:.0f} MiB", change(sa, sb, "mem")))
    row("Startup to ready", lambda r: r["startup_ms"], fmt="{:.0f}")
    classes = [("golden", "Golden corpus routes"), ("random", "Random routes, unseeded"), ("random_seeded", "Random routes, seeded"), ("all", "All routes")]
    for cls, name in classes:
        for q in ("p50", "p90", "p99", "mean"):
            row(f"{name} — {q} (client, cache off)", lambda r, c=cls, q=q: r["latency"].get(c, {}).get(q))
    for cls, name in classes[:3]:
        row(f"{name} — p50 server search time", lambda r, c=cls: r["server_ms"].get(c, {}).get("p50"), fmt="{:.3f}")
        row(f"{name} — p99 server search time", lambda r, c=cls: r["server_ms"].get(c, {}).get("p99"), fmt="{:.3f}")
    row("Cache hit — p50", lambda r: r["cache_hit"]["p50"], fmt="{:.3f}")
    row("Cache hit — p99", lambda r: r["cache_hit"]["p99"], fmt="{:.3f}")
    c = args.concurrency
    row(f"{c} concurrent clients — throughput", lambda r: r["concurrency"]["throughput_rps"], unit="req/s", kind="rate", fmt="{:.0f}")
    row(f"{c} concurrent clients — p50", lambda r: r["concurrency"]["p50"])
    row(f"{c} concurrent clients — p99", lambda r: r["concurrency"]["p99"])
    row(f"{c} concurrent clients — 503 rejections", lambda r: r["concurrency"]["rejects_503"], unit="", kind="count", fmt="{:.0f}")
    row("Resident memory after run (cache off)", lambda r: r["mem_nocache"].get("VmRSS"), unit="MiB", kind="mem", fmt="{:.0f}")
    row("Resident memory after run (cache on)", lambda r: r["mem_cache"].get("VmRSS"), unit="MiB", kind="mem", fmt="{:.0f}")

    lines = [
        "# NavPath latency comparison",
        "",
        f"Generated {time.strftime('%Y-%m-%d %H:%M')} by `tools/latency_compare.py` — {args.rounds} alternating round(s) per build, medians shown.",
        "",
        f"- **Baseline:** `{args.base_bin}` + `{args.base_snapshot}`",
        f"- **New:** `{args.new_bin}` + `{args.new_snapshot}`",
        f"- **Environment (both):** {', '.join(f'{k}={v}' for k, v in sorted(env_extra.items()))}",
        f"- **Requests:** {sum(1 for c_, _ in reqs if c_ == 'golden')} golden-corpus routes, {args.random} random unseeded, {args.seeded} random seeded (all with a rich requirement profile, geometry + actions, surge/dive on).",
        f"- **Answer check:** {mism} of {len(reqs)} requests returned a different found/cost between the builds.",
        "",
        "| Metric | Baseline | New | Change |",
        "|---|---|---|---|",
    ]
    for name, a, b, ch in rows:
        lines.append(f"| {name} | {a} | {b} | {ch} |")
    lines.append("")
    with open(args.out, "w") as f:
        f.write("\n".join(lines))
    raw = args.out.rsplit(".", 1)[0] + ".json"
    for side in runs.values():
        for r in side:
            r.pop("answers", None)
    with open(raw, "w") as f:
        json.dump(runs, f, indent=1)
    print("\n".join(lines))
    return 0 if mism == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
