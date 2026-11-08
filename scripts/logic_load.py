#!/usr/bin/env python3
"""A logic workload measured on stated hardware (FLC-QUAL-004), and fairness between workspaces (FLC-OPS-002,
-003): cold, warm, concurrent and failure scenarios against one running build, through its HTTP API as
ordinary roles, with costs kept apart: compiling a source, simulating it, and running it on the runtime.

    LOGIC_LOAD_RESTART="zsh ERP/flc/walk/restart_server.sh" scripts/logic_load.py http://127.0.0.1:5170 acme globex

Needs `scripts/lrp_walk.py` to have run first (its procurement solution active in both workspaces, its people).
LOGIC_LOAD_RESTART, when set, is a command that stops and starts the server; the failure scenario uses it.
Prints each measurement, and `ok`/`FAIL` for the bounds it checks; exits 1 if any failed.
"""

import concurrent.futures as cf
import json
import os
import platform
import statistics
import subprocess
import sys
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lrp_walk as w  # noqa: E402

BASE, HOME, AWAY = w.BASE, w.HOME, w.AWAY
SID = "workspace.lrp_procurement"
FAILED = w.FAILED


def stats(name, ms):
    ms = sorted(ms)
    p95 = ms[max(0, int(len(ms) * 0.95) - 1)]
    print(f"     {name}: n={len(ms)} median {statistics.median(ms):.0f} ms, p95 {p95:.0f} ms, max {ms[-1]:.0f} ms", flush=True)
    return p95


def orders_for(who, request):
    q = urllib.parse.quote(json.dumps([["request", "=", request]]))
    d = who("GET", f"/api/v1/Order?filters={q}")[1]
    return w.rows(d)


def one(p, ref, decide=True):
    """Submit a request; time until its review opens, and from the decision to its order. (ms, ms, request id)."""
    t0 = time.perf_counter()
    s, req = w.create(p["requester"], "Request", {"reference": ref, "total": "2500"})
    p["requester"]("POST", f"/api/v1/Request/{req['id']}/submit", {})
    run = w.run_of(p["admin"], SID, req["id"])
    task = w.task_of(p["admin"], run["id"], "review") if run else None
    opened = (time.perf_counter() - t0) * 1000
    if not decide or not task:
        return opened, None, req["id"]
    t1 = time.perf_counter()
    w.decide(p["reviewer"], task["id"])
    w.eventually(lambda: orders_for(p["requester"], req["id"]), 120, 0.2)
    return opened, (time.perf_counter() - t1) * 1000, req["id"]


def people(tenant):
    admin = w.Person(tenant, f"admin@{tenant}.test")
    return {"admin": admin, "requester": w.Person(tenant, f"requester@{tenant}.test"),
            "reviewer": w.person(admin, f"reviewer@{tenant}.test", ["Reviewer"])}


def main():
    print(f"Hardware: {platform.platform()}, {os.cpu_count()} cores, Python {platform.python_version()}; server {BASE}")
    home, away = people(HOME), people(AWAY)
    author = w.Person(HOME, f"author@{HOME}.test")
    source = json.dumps(w.procurement(SID, "Load", "9.9.9"))
    print("\n== Source and planner costs (nothing runs)")
    compile_ms, sim_ms = [], []
    for _ in range(10):
        t0 = time.perf_counter()
        author("POST", f"{w.STUDIO}/solutions/compile", {"source": source})
        compile_ms.append((time.perf_counter() - t0) * 1000)
    scenario = {"process": "approve_request", "trigger": {"id": "R-1"}, "start": "2026-11-09T09:00:00Z",
                "results": {"snapshot": {"result": "done", "output": {"total": "2500"}}, "order": {"result": "done", "output": {"id": "O-1"}}},
                "decisions": {"review": {"approve": True}}, "signals": {"delivered": {"payload": {"id": "O-1"}}}}
    for _ in range(10):
        t0 = time.perf_counter()
        author("POST", f"{w.STUDIO}/solutions/simulate", {"source": source, "scenario": scenario})
        sim_ms.append((time.perf_counter() - t0) * 1000)
    stats("compile a source (HTTP round trip included)", compile_ms)
    stats("simulate a run to its end on the virtual clock", sim_ms)

    print("\n== Cold: the first run after the server started")
    opened, decided, _ = one(home, f"COLD-{w.RUN}")
    print(f"     review open after {opened:.0f} ms; order {decided:.0f} ms after the decision")

    print("\n== Warm: ten in turn, in each workspace")
    base_open, base_order = {}, {}
    for name, p in ((HOME, home), (AWAY, away)):
        got = [one(p, f"WARM-{name}-{w.RUN}-{i}") for i in range(10)]
        base_open[name] = stats(f"{name}: submitted to review open", [g[0] for g in got])
        base_order[name] = stats(f"{name}: decided to ordered", [g[1] for g in got])

    print(f"\n== Concurrent and fair: {HOME} floods 60 at once while {AWAY} asks for 5")
    with cf.ThreadPoolExecutor(max_workers=48) as pool:
        t0 = time.perf_counter()
        flood = [pool.submit(one, home, f"FLOOD-{w.RUN}-{i}") for i in range(60)]
        time.sleep(1.0)
        few = [pool.submit(one, away, f"FEW-{w.RUN}-{i}") for i in range(5)]
        flood_got = [f.result() for f in flood]
        few_got = [f.result() for f in few]
        total = (time.perf_counter() - t0)
    stats(f"{HOME} under its own flood: decided to ordered", [g[1] for g in flood_got if g[1] is not None])
    away_p95 = stats(f"{AWAY} beside the flood: decided to ordered", [g[1] for g in few_got if g[1] is not None])
    print(f"     65 runs approved and ordered in {total:.1f} s ({65 / total:.1f} runs/s end to end)")
    w.check(f"every flooded run ordered exactly once", all(len(orders_for(home["requester"], g[2])) == 1 for g in flood_got), None)
    w.check(f"{AWAY}'s orders beside the flood stay within 10 s at p95", away_p95 < 10_000, away_p95)
    s, metrics = 0, ""
    try:
        import urllib.request
        metrics = urllib.request.urlopen(BASE + "/metrics", timeout=10).read().decode()
    except Exception as e:  # noqa: BLE001
        metrics = str(e)
    lines = [l for l in metrics.splitlines() if l.startswith("ferrum_logic_steps")]
    print("     " + "\n     ".join(lines))
    w.check("the worker's step budget is on /metrics", any(l.startswith("ferrum_logic_steps_limit") for l in lines), lines)

    restart = os.environ.get("LOGIC_LOAD_RESTART")
    print("\n== Failure: the server stops while runs are in hand, and comes back")
    if not restart:
        print("     (LOGIC_LOAD_RESTART not set: not run)")
    else:
        refs = []
        with cf.ThreadPoolExecutor(max_workers=16) as pool:
            submitted = [pool.submit(one, home, f"CRASH-{w.RUN}-{i}", False) for i in range(15)]
            refs = [f.result()[2] for f in submitted]
        tasks = []
        for r in refs:
            run = w.run_of(home["admin"], SID, r)
            tasks.append(w.task_of(home["admin"], run["id"], "review")["id"])
        with cf.ThreadPoolExecutor(max_workers=16) as pool:
            list(pool.map(lambda t: w.decide(home["reviewer"], t), tasks))
        t0 = time.perf_counter()
        subprocess.run(restart, shell=True, check=True)
        home = people(HOME)
        done = w.eventually(lambda: all(len(orders_for(home["requester"], r)) >= 1 for r in refs), 180, 1)
        print(f"     back and settled {time.perf_counter() - t0:.1f} s after the restart began")
        w.check("every run decided before the stop ordered, after it", done, refs)
        w.check("and none ordered twice", all(len(orders_for(home["requester"], r)) == 1 for r in refs), None)
    print(f"\n{len(FAILED)} failed" if FAILED else "\nevery bound held")
    sys.exit(1 if FAILED else 0)


if __name__ == "__main__":
    main()
