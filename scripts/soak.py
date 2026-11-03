#!/usr/bin/env python3
"""J12 in software: hours of sustained operation on one Compose stack, as far as one machine can say.

Three workspaces of different weight (heavy, steady, light) work the server at once, creating and submitting
invoices so that their data grows. Along the way the database restarts, a backup is taken and read back, and the
server's container is replaced as an upgrade replaces it. Every window it records each workspace's requests,
refusals, errors and latency, and every half minute the server's memory, the database's size and the outbox.
At the end it checks that:

- outside an outage, errors stay under half a percent, and the light workspace is never refused;
- the light workspace is not starved while the heavy one floods (the per-workspace share);
- the service is back within two minutes of each outage, and the backup is taken and read back while it serves;
- the server's memory does not keep growing while one container runs;
- the outbox drains once the load stops, and nothing more died in it.

It writes soak.json and soak.md for the evidence pack and exits 1 when a check fails. The stack and the
workspaces are the caller's (.github/workflows/soak.yml): slugs heavy, steady and light, each signing in as
soak@<slug>.test with the same password, seeded with seed_demo.

    python3 scripts/soak.py --url http://127.0.0.1:8080 --password ... --hours 3 \\
      --compose "docker compose -f deploy/docker-compose.yml"
"""

import argparse
import asyncio
import json
import random
import re
import shlex
import subprocess
import sys
import time
import uuid
from collections import defaultdict
from pathlib import Path
from types import SimpleNamespace

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import loadtest  # noqa: E402  the calls the load test makes, and its percentile

WORKERS = {"heavy": 24, "steady": 6, "light": 2}
INCIDENTS = [(0.25, "database restart"), (0.5, "backup"), (0.75, "server replaced")]
# What takes the service away for a moment, so that errors then are expected. A backup does not.
OUTAGES = {"database restart", "server replaced"}
RECOVERY_SECS = 120
UNIT = {"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30, "kB": 1000, "MB": 1000**2, "GB": 1000**3}


def run(cmd, timeout=900):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(cmd, 124, "", f"took longer than {timeout} s")


class Soak:
    def __init__(self, args):
        self.args = args
        self.compose = shlex.split(args.compose)
        self.duration = args.hours * 3600
        self.window = min(600.0, max(60.0, self.duration / 12))
        self.windows = defaultdict(lambda: defaultdict(lambda: {"n": 0, "errors": 0, "refused": 0, "lat": []}))
        self.errors = defaultdict(list)  # workspace -> times of errors, for telling them from incidents
        self.samples = []
        self.incidents = []
        self.t0 = 0.0

    # ── what one workspace does ──
    async def workspace(self, slug):
        client = httpx.AsyncClient(base_url=self.args.url, timeout=60, limits=httpx.Limits(max_connections=100))
        who = SimpleNamespace(tenant=slug, email=f"soak@{slug}.test", password=self.args.password)
        client.headers["authorization"] = f"Bearer {await loadtest.sign_in(client, who)}"
        company, customer, items = await loadtest.fixtures(client)
        mix = loadtest.calls(company, customer, items, [])

        async def submit_invoice(c):
            lines = [{"item": random.choice(items), "qty": str(random.randint(1, 5))} for _ in range(random.randint(1, 3))]
            return await c.post(
                "/api/v1/SalesInvoice",
                params={"submit": "true"},
                json={"company": company, "customer": customer, "items": lines},
                headers={"idempotency-key": f"soak-{uuid.uuid4()}"},
            )

        mix.append(("submit invoice", 5, submit_invoice))
        return client, mix

    async def work(self, slug, client, mix, until, record=True, latencies=None):
        names, weights, funcs = [m[0] for m in mix], [m[1] for m in mix], {m[0]: m[2] for m in mix}
        while time.monotonic() < until:
            name = random.choices(names, weights)[0]
            started = time.monotonic()
            try:
                status = (await funcs[name](client)).status_code
            except httpx.HTTPError:
                status = 0
            ms = (time.monotonic() - started) * 1000
            if latencies is not None:
                latencies.append(ms)
            if not record:
                continue
            w = self.windows[int((started - self.t0) // self.window)][slug]
            w["n"] += 1
            if status == 429:
                w["refused"] += 1
            elif status == 0 or status >= 400:
                w["errors"] += 1
                self.errors[slug].append(started - self.t0)
            else:
                w["lat"].append(ms)
            if status in (0, 429) or status >= 500:
                await asyncio.sleep(0.2)  # a refused or failing server is not hammered in a tight loop

    # ── what the stack is doing ──
    def server(self):
        out = run([*self.compose, "ps", "-q", "ferrum"], 60).stdout.split()
        return out[0] if out else None

    def memory(self, container):
        out = run(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", container], 60).stdout
        m = re.match(r"\s*([\d.]+)\s*([A-Za-z]+)", out)
        return int(float(m.group(1)) * UNIT.get(m.group(2), 1)) if m else None

    def database_bytes(self):
        out = run([*self.compose, "exec", "-T", "db", "psql", "-U", "postgres", "-d", "ferrum", "-tAc", "select pg_database_size('ferrum')"], 120)
        return int(out.stdout.strip()) if out.returncode == 0 and out.stdout.strip().isdigit() else None

    async def metrics(self):
        try:
            async with httpx.AsyncClient(base_url=self.args.url, timeout=20) as c:
                text = (await c.get("/metrics")).text
        except httpx.HTTPError:
            return {}
        return {m.group(1): float(m.group(2)) for m in re.finditer(r"^(ferrum_outbox_pending|ferrum_outbox_dead|ferrum_tenant_refused_total) (\S+)$", text, re.M)}

    async def sample(self):
        container = await asyncio.to_thread(self.server)
        memory = await asyncio.to_thread(self.memory, container) if container else None
        db = await asyncio.to_thread(self.database_bytes)
        found = await self.metrics()
        self.samples.append({"at": round(time.monotonic() - self.t0, 1), "container": container, "memory": memory, "database": db, **found})

    async def sampler(self, until):
        while time.monotonic() < until:
            await self.sample()
            await asyncio.sleep(30)

    async def ready(self, client):
        try:
            return (await client.get("/api/ready")).status_code == 200
        except httpx.HTTPError:
            return False

    async def incident(self, kind):
        start = time.monotonic()
        detail = ""
        if kind == "database restart":
            done = await asyncio.to_thread(run, [*self.compose, "restart", "db"], 300)
        elif kind == "server replaced":
            done = await asyncio.to_thread(run, [*self.compose, "up", "-d", "--no-build", "--no-deps", "--force-recreate", "ferrum"], 300)
        else:
            script = "pg_dump -U postgres -Fc ferrum > /tmp/soak.dump && pg_restore --list /tmp/soak.dump | grep -c TABLE; rm -f /tmp/soak.dump"
            done = await asyncio.to_thread(run, [*self.compose, "exec", "-T", "db", "sh", "-c", script], 1800)
            detail = f"{done.stdout.strip() or 0} tables in the backup, read back"
        ok = done.returncode == 0 and (kind != "backup" or int((done.stdout.strip() or "0").splitlines()[-1]) > 50)
        async with httpx.AsyncClient(base_url=self.args.url, timeout=5) as c:
            while not await self.ready(c) and time.monotonic() - start < 600:
                await asyncio.sleep(0.5)
            back = await self.ready(c)
        seconds = time.monotonic() - start
        self.incidents.append(
            {"kind": kind, "at": round(start - self.t0, 1), "seconds": round(seconds, 1), "ok": ok and back, "detail": detail or done.stderr.strip()[-300:]}
        )

    async def incidents_on_schedule(self):
        for fraction, kind in INCIDENTS:
            await asyncio.sleep(max(0.0, self.t0 + fraction * self.duration - time.monotonic()))
            await self.incident(kind)

    # ── the whole of it ──
    async def main(self):
        sessions = {slug: await self.workspace(slug) for slug in WORKERS}
        # What the light workspace gets with the server to itself.
        alone = []
        client, mix = sessions["light"]
        until = time.monotonic() + 60
        await asyncio.gather(*(self.work("light", client, mix, until, record=False, latencies=alone) for _ in range(WORKERS["light"])))
        baseline = loadtest.percentile(alone, 0.95)
        dead_before = (await self.metrics()).get("ferrum_outbox_dead", 0)

        self.t0 = time.monotonic()
        until = self.t0 + self.duration
        work = [self.work(slug, *sessions[slug], until) for slug, n in WORKERS.items() for _ in range(n)]
        await asyncio.gather(self.sampler(until), self.incidents_on_schedule(), *work)

        # The load has stopped: what is waiting in the outbox goes out.
        drained_at = None
        for _ in range(36):
            await self.sample()
            if self.samples[-1].get("ferrum_outbox_pending") == 0:
                drained_at = self.samples[-1]["at"]
                break
            await asyncio.sleep(5)
        for client, _ in sessions.values():
            await client.aclose()
        return self.report(baseline, dead_before, drained_at)

    def report(self, baseline, dead_before, drained_at):
        incident_spans = [(i["at"], i["at"] + i["seconds"] + 15) for i in self.incidents]
        outage_spans = [(i["at"], i["at"] + i["seconds"] + 15) for i in self.incidents if i["kind"] in OUTAGES]
        during = lambda t: any(a <= t <= b for a, b in outage_spans)  # noqa: E731
        quiet_windows = [
            w for w in sorted(self.windows) if not any(a < (w + 1) * self.window and b > w * self.window for a, b in incident_spans)
        ]
        checks = []

        def check(name, ok, said):
            checks.append({"check": name, "ok": bool(ok), "said": said})

        for slug in WORKERS:
            total = sum(self.windows[w][slug]["n"] for w in self.windows)
            outside = sum(1 for t in self.errors[slug] if not during(t))
            check(f"{slug}: errors outside outages", total > 0 and outside <= 0.005 * total, f"{outside} of {total} requests")
        light_refused = sum(self.windows[w]["light"]["refused"] for w in self.windows)
        check("light: never refused", light_refused == 0, f"{light_refused} refused")
        worst = max((loadtest.percentile(self.windows[w]["light"]["lat"], 0.95) for w in quiet_windows), default=0.0)
        # Starved, not slower: the database's CPU is everyone's (docs/operations.md), the turns are not.
        check(
            "light: not starved while heavy floods",
            quiet_windows and worst <= 4 * baseline + 100,
            f"p95 {worst:.0f} ms at worst in {len(quiet_windows)} quiet windows, {baseline:.0f} ms alone",
        )
        for i in self.incidents:
            if i["kind"] in OUTAGES:
                check(f"recovered from: {i['kind']}", i["ok"] and i["seconds"] <= RECOVERY_SECS, f"back after {i['seconds']} s")
            else:
                check(f"{i['kind']}: taken and read back while serving", i["ok"], f"{i['detail']}, in {i['seconds']} s")
        check("every incident happened", len(self.incidents) == len(INCIDENTS), f"{len(self.incidents)} of {len(INCIDENTS)}")
        # Memory, within the container that ran longest, after its first five minutes.
        by = defaultdict(list)
        for s in self.samples:
            if s["container"] and s["memory"]:
                by[s["container"]].append(s)
        longest = max(by.values(), key=lambda v: v[-1]["at"] - v[0]["at"], default=[])
        settled = [s for s in longest if s["at"] - longest[0]["at"] >= 300]
        if len(settled) >= 4:
            first, last = settled[0]["memory"], settled[-1]["memory"]
            check("server memory does not keep growing", last <= max(1.5 * first, first + (200 << 20)), f"{first >> 20} MiB to {last >> 20} MiB")
        else:
            check("server memory does not keep growing", True, "too short a run to say")
        dead_after = self.samples[-1].get("ferrum_outbox_dead", 0)
        check("outbox drains once the load stops", drained_at is not None, f"empty at {drained_at} s" if drained_at else "still waiting after 3 minutes")
        check("nothing more died in the outbox", dead_after <= dead_before, f"{dead_before:.0f} dead before, {dead_after:.0f} after")

        rows = []
        for w in sorted(self.windows):
            cells = []
            for slug in WORKERS:
                s = self.windows[w][slug]
                cells.append(f"{s['n']} / {s['errors']} / {s['refused']} / {loadtest.percentile(s['lat'], 0.95):.0f}")
            inside = [x for x in self.samples if w * self.window <= x["at"] < (w + 1) * self.window]
            last = inside[-1] if inside else {}
            memory = f"{last['memory'] >> 20}" if last.get("memory") else ""
            database = f"{last['database'] >> 20}" if last.get("database") else ""
            rows.append([f"{w * self.window / 60:.0f}", *cells, memory, database, f"{last.get('ferrum_outbox_pending', '')}"])
        out = {"hours": self.args.hours, "image": self.args.image, "window_secs": self.window, "baseline_p95_ms": baseline, "checks": checks,
               "incidents": self.incidents, "samples": self.samples,
               "windows": {w: {s: {k: v for k, v in d.items() if k != "lat"} | {"p95": loadtest.percentile(d["lat"], 0.95)} for s, d in self.windows[w].items()} for w in sorted(self.windows)}}
        Path("soak.json").write_text(json.dumps(out, indent=1))
        lines = [
            f"# Soak: {self.args.hours} h on {self.args.image}",
            "",
            "Three workspaces at once on one Compose stack on one machine (a CI runner): what it says is about that, not a deployment.",
            "",
            "| Check | | |",
            "|---|---|---|",
            *[f"| {c['check']} | {'pass' if c['ok'] else '**FAIL**'} | {c['said']} |" for c in checks],
            "",
            "| Incident | At (min) | Back after (s) | |",
            "|---|---|---|---|",
            *[f"| {i['kind']} | {i['at'] / 60:.0f} | {i['seconds']} | {i['detail']} |" for i in self.incidents],
            "",
            "Each workspace: requests / errors / refused / p95 ms.",
            "",
            "| From (min) | Heavy | Steady | Light | Server MiB | Database MiB | Outbox |",
            "|---|---|---|---|---|---|---|",
            *["| " + " | ".join(r) + " |" for r in rows],
        ]
        Path("soak.md").write_text("\n".join(lines) + "\n")
        print("\n".join(lines))
        return 0 if all(c["ok"] for c in checks) else 1


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--url", required=True)
    p.add_argument("--password", required=True)
    p.add_argument("--hours", type=float, default=3)
    p.add_argument("--compose", default="docker compose -f deploy/docker-compose.yml")
    p.add_argument("--image", default="ghcr.io/cbferrum/ferrum:latest", help="what ran, for the report")
    sys.exit(asyncio.run(Soak(p.parse_args()).main()))


if __name__ == "__main__":
    main()
