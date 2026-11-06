#!/usr/bin/env python3
"""Load test for a Ferrum server: a mix of list, read, metadata, live-total and create calls by
concurrent clients of one workspace, at rising concurrency, reporting throughput, errors and
latency percentiles per call.

    pip install httpx
    FERRUM_URL=https://erp.example.com FERRUM_TENANT=acme FERRUM_EMAIL=load@acme.example \
      FERRUM_PASSWORD=... python3 tools/loadtest.py --levels 10,50,100 --seconds 30

Run it against a staging copy, never production: it creates draft Sales Invoices (deleted at the
end unless --keep). The workspace needs a company, a customer and two priced items; the script
picks them. Exit status 1 when any level has more than 1% errors, or misses a target it was given:
`--p95-ms 10:300,50:2000` (the most a level's p95 may take) and `--min-rps 10:120,50:100` (the
fewest requests a second it must serve). This mix, at those levels, is the reference workload
(docs/operations.md, "Reference workload and targets"); every image build runs it (deploy/smoke.sh).
`--summary` writes each level's figures as JSON.
"""

import argparse
import asyncio
import json
import os
import random
import statistics
import sys
import time
from collections import defaultdict

import httpx


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    k = (len(values) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (k - lo)


async def sign_in(client, args):
    r = await client.post("/api/auth/login", json={"tenant": args.tenant, "email": args.email, "password": args.password})
    r.raise_for_status()
    body = r.json()
    if "token" not in body or body.get("two_factor_setup"):
        sys.exit("the load-test account must sign in with a password alone (no two-factor step)")
    return body["token"]


async def fixtures(client):
    """A company, a customer and items to invoice."""
    async def first(entity, query):
        r = await client.get(f"/api/v1/{entity}", params=query)
        r.raise_for_status()
        rows = r.json()["rows"]
        if not rows:
            sys.exit(f"the workspace needs at least one {entity}")
        return rows

    company = (await first("Company", {"limit": 1, "fields": "id"}))[0]["id"]
    customer = (await first("Customer", {"limit": 1, "fields": "id", "filters": '{"disabled": false}'}))[0]["id"]
    items = [r["id"] for r in await first("Item", {"limit": 20, "fields": "id", "filters": '{"disabled": false}'})]
    return company, customer, items


def calls(company, customer, items, created):
    """The mix: (name, weight, coroutine factory taking the client)."""

    def invoice_body():
        lines = [{"item": random.choice(items), "qty": str(random.randint(1, 5))} for _ in range(random.randint(1, 3))]
        return {"company": company, "customer": customer, "items": lines}

    async def list_customers(c):
        return await c.get("/api/v1/Customer", params={"limit": 20, "fields": "id,customer_name,customer_group"})

    async def list_invoices(c):
        return await c.get("/api/v1/SalesInvoice", params={"limit": 20, "sort": "posting_date desc", "titles": "true"})

    async def read_item(c):
        return await c.get(f"/api/v1/Item/{random.choice(items)}")

    async def meta(c):
        return await c.get("/api/meta/SalesInvoice")

    async def live_total(c):
        return await c.post("/api/v1/SalesInvoice/_calc", json=invoice_body())

    async def create_draft(c):
        r = await c.post("/api/v1/SalesInvoice", json=invoice_body())
        if r.status_code == 201:
            created.append(r.json()["id"])
        return r

    return [
        ("list customers", 30, list_customers),
        ("list invoices", 20, list_invoices),
        ("read item", 20, read_item),
        ("metadata", 10, meta),
        ("live totals", 10, live_total),
        ("create draft invoice", 10, create_draft),
    ]


async def level(client, mix, concurrency, seconds):
    names = [m[0] for m in mix]
    weights = [m[1] for m in mix]
    funcs = {m[0]: m[2] for m in mix}
    latencies = defaultdict(list)
    errors = defaultdict(int)
    samples = defaultdict(list)
    deadline = time.monotonic() + seconds

    async def worker():
        while time.monotonic() < deadline:
            name = random.choices(names, weights)[0]
            start = time.perf_counter()
            try:
                r = await funcs[name](client)
                ok = r.status_code < 400
                if not ok and len(samples[name]) < 3:
                    samples[name].append(f"{r.status_code} {r.text[:160]}")
            except httpx.HTTPError as e:
                ok = False
                if len(samples[name]) < 3:
                    samples[name].append(repr(e)[:160])
            latencies[name].append((time.perf_counter() - start) * 1000)
            if not ok:
                errors[name] += 1

    started = time.monotonic()
    await asyncio.gather(*(worker() for _ in range(concurrency)))
    elapsed = time.monotonic() - started
    total = sum(len(v) for v in latencies.values())
    failed = sum(errors.values())
    every = [x for v in latencies.values() for x in v]
    print(f"\nconcurrency {concurrency}: {total} requests in {elapsed:.0f} s = {total / elapsed:.0f}/s, "
          f"errors {failed} ({100 * failed / max(total, 1):.2f}%), p50 {percentile(every, 0.5):.0f} ms, "
          f"p95 {percentile(every, 0.95):.0f} ms, p99 {percentile(every, 0.99):.0f} ms")
    print(f"  {'call':<22}{'count':>7}{'errors':>8}{'mean':>8}{'p50':>7}{'p95':>7}{'p99':>7}  (ms)")
    for name in names:
        v = latencies[name]
        if v:
            print(f"  {name:<22}{len(v):>7}{errors[name]:>8}{statistics.mean(v):>8.0f}{percentile(v, 0.5):>7.0f}"
                  f"{percentile(v, 0.95):>7.0f}{percentile(v, 0.99):>7.0f}")
        for s in samples[name]:
            print(f"    e.g. {s}")
    return {"concurrency": concurrency, "requests": total, "seconds": round(elapsed, 1), "rps": round(total / elapsed, 1),
            "error_rate": round(failed / max(total, 1), 4), "p50_ms": round(percentile(every, 0.5), 1),
            "p95_ms": round(percentile(every, 0.95), 1), "p99_ms": round(percentile(every, 0.99), 1)}


def targets(spec):
    """`10:300,50:2000` -> {10: 300.0, 50: 2000.0}."""
    out = {}
    for part in (spec or "").split(","):
        if part.strip():
            level, value = part.split(":")
            out[int(level)] = float(value)
    return out


def missed(result, p95_max, rps_min):
    """What a level missed of its targets, as sentences."""
    n, out = result["concurrency"], []
    if result["error_rate"] > 0.01:
        out.append(f"{n} clients: {100 * result['error_rate']:.2f}% of requests failed, more than 1%")
    if n in p95_max and result["p95_ms"] > p95_max[n]:
        out.append(f"{n} clients: p95 {result['p95_ms']:.0f} ms, more than {p95_max[n]:.0f}")
    if n in rps_min and result["rps"] < rps_min[n]:
        out.append(f"{n} clients: {result['rps']:.0f} requests a second, fewer than {rps_min[n]:.0f}")
    return out


async def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--url", default=os.environ.get("FERRUM_URL", "http://localhost:5150"))
    p.add_argument("--tenant", default=os.environ.get("FERRUM_TENANT"))
    p.add_argument("--email", default=os.environ.get("FERRUM_EMAIL"))
    p.add_argument("--password", default=os.environ.get("FERRUM_PASSWORD"))
    p.add_argument("--levels", default="10,50,100", help="concurrency levels, comma-separated")
    p.add_argument("--seconds", type=int, default=30, help="duration of each level")
    p.add_argument("--keep", action="store_true", help="keep the draft invoices it creates")
    p.add_argument("--p95-ms", default="", help="targets: the most each level's p95 may take, e.g. 10:300,50:2000")
    p.add_argument("--min-rps", default="", help="targets: the fewest requests a second each level must serve, e.g. 10:120,50:100")
    p.add_argument("--summary", help="write each level's figures here, as JSON")
    args = p.parse_args()
    p95_max, rps_min = targets(args.p95_ms), targets(args.min_rps)
    if not (args.tenant and args.email and args.password):
        sys.exit("set FERRUM_TENANT, FERRUM_EMAIL and FERRUM_PASSWORD (or --tenant/--email/--password)")
    limits = httpx.Limits(max_connections=1000, max_keepalive_connections=1000)
    async with httpx.AsyncClient(base_url=args.url, timeout=60, limits=limits) as client:
        client.headers["authorization"] = f"Bearer {await sign_in(client, args)}"
        company, customer, items = await fixtures(client)
        created = []
        mix = calls(company, customer, items, created)
        results, misses = [], []
        for n in [int(x) for x in args.levels.split(",") if x.strip()]:
            result = await level(client, mix, n, args.seconds)
            results.append(result)
            misses.extend(missed(result, p95_max, rps_min))
        if created and not args.keep:
            gone = 0
            for i in range(0, len(created), 50):
                results = await asyncio.gather(*(client.delete(f"/api/v1/SalesInvoice/{d}") for d in created[i:i + 50]), return_exceptions=True)
                gone += sum(1 for r in results if not isinstance(r, Exception) and r.status_code == 204)
            print(f"\ndeleted {gone} of {len(created)} draft invoices")
    if args.summary:
        with open(args.summary, "w") as f:
            json.dump({"levels": results, "targets": {"p95_ms": p95_max, "min_rps": rps_min}, "missed": misses}, f, indent=2)
    for m in misses:
        print(f"MISSED {m}")
    sys.exit(1 if misses else 0)


if __name__ == "__main__":
    asyncio.run(main())
