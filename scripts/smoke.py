#!/usr/bin/env python3
"""Post-deploy smoke and concurrency check against a running Ferrum (stdlib only).

    FERRUM_URL=https://erp.example.com FERRUM_TENANT=acme FERRUM_EMAIL=admin@acme.com \
    FERRUM_PASSWORD=... python3 scripts/smoke.py

Creates a throw-away item and company stock in the tenant's first company, so run it against a
staging or demo tenant. Checks: readiness, login, reports, idempotent creates, optimistic locking,
and that concurrent stock issues never overdraw stock and never fail with a server error.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor

BASE = os.environ.get("FERRUM_URL", "http://127.0.0.1:5150").rstrip("/")
TENANT = os.environ.get("FERRUM_TENANT", "demo")
EMAIL = os.environ.get("FERRUM_EMAIL", "admin@demo.test")
PASSWORD = os.environ.get("FERRUM_PASSWORD", "Correct-Horse-9")
WORKERS = int(os.environ.get("FERRUM_WORKERS", "24"))
TOKEN = None


def call(method, path, body=None, headers=None, attempts=6):
    """One API call. 503 means "overloaded or failing over, retry": honour Retry-After, as clients must."""
    for attempt in range(attempts):
        status, h, b = _call(method, path, body, headers)
        if status != 503 or attempt == attempts - 1:
            return status, h, b
        time.sleep(float(next((v for k, v in h.items() if k.lower() == "retry-after"), "1")))
    return status, h, b


def _call(method, path, body=None, headers=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("content-type", "application/json")
    if TOKEN:
        req.add_header("authorization", f"Bearer {TOKEN}")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            raw = r.read()
            return r.status, dict(r.headers), (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, dict(e.headers), json.loads(raw)
        except ValueError:
            return e.code, dict(e.headers), raw.decode(errors="replace")


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        sys.exit(1)


def first(entity, filters=None):
    q = "?limit=1" + ("&filters=" + urllib.request.quote(json.dumps(filters)) if filters else "")
    s, _, b = call("GET", f"/api/v1/{entity}{q}")
    check(s == 200 and b["rows"], f"found a {entity}")
    return b["rows"][0]


def main():
    global TOKEN
    s, _, _ = call("GET", "/api/ready")
    check(s == 200, "server ready")
    s, _, b = call("POST", "/api/auth/login", {"tenant": TENANT, "email": EMAIL, "password": PASSWORD})
    check(s == 200, "login")
    TOKEN = b["token"]
    s, _, b = call("GET", "/api/reports/trial_balance")
    check(s == 200 and b["meta"]["balanced"], "trial balance balances")

    company = first("Company")
    group = first("ItemGroup", {"group_name": "Raw Material"})
    uom = first("Uom", {"uom_name": "Nos"})
    s, _, wh = call("GET", "/api/v1/Warehouse?limit=1&filters=" + urllib.request.quote(json.dumps({"warehouse_name": "Stores", "company": company["id"]})))
    store = wh["rows"][0]["id"]
    code = "SMOKE-" + uuid.uuid4().hex[:8].upper()
    s, _, item = call("POST", "/api/v1/Item", {"item_code": code, "item_name": "Smoke test item", "item_group": group["id"], "stock_uom": uom["id"]})
    check(s == 201, f"created item {code}")

    # Idempotency: the same key returns the same record, flagged as a replay.
    key = str(uuid.uuid4())
    receipt = {"company": company["id"], "purpose": "Material Receipt", "items": [{"item": item["id"], "qty": "100", "basic_rate": "10", "target_warehouse": store}]}
    s1, _, r1 = call("POST", "/api/v1/StockEntry", receipt, {"Idempotency-Key": key})
    s2, h2, r2 = call("POST", "/api/v1/StockEntry", receipt, {"Idempotency-Key": key})
    check(s1 == 201 and s2 == 200 and r1["id"] == r2["id"], "idempotent create replays the original")
    check(any(k.lower() == "idempotent-replayed" for k in h2), "replay is flagged")

    # Optimistic locking: a stale version is refused.
    s, _, _ = call("PATCH", f"/api/v1/StockEntry/{r1['id']}", {"remarks": "first"}, {"If-Match": '"v1"'})
    check(s == 200, "update with the current version")
    s, _, _ = call("PATCH", f"/api/v1/StockEntry/{r1['id']}", {"remarks": "stale"}, {"If-Match": '"v1"'})
    check(s == 409, "stale update refused with 409")
    s, _, _ = call("POST", f"/api/v1/StockEntry/{r1['id']}/submit")
    check(s == 200, "receipt of 100 units submitted")

    # 60 concurrent issues of 2 units race for 100 units: exactly 50 may succeed.
    def issue(_):
        s, _, d = call("POST", "/api/v1/StockEntry", {"company": company["id"], "purpose": "Material Issue",
                                                     "items": [{"item": item["id"], "qty": "2", "source_warehouse": store}]})
        if s != 201:
            return s
        return call("POST", f"/api/v1/StockEntry/{d['id']}/submit")[0]

    started = time.time()
    with ThreadPoolExecutor(max_workers=WORKERS) as pool:
        results = list(pool.map(issue, range(60)))
    elapsed = time.time() - started
    ok = results.count(200)
    refused = sum(1 for r in results if r in (409, 422))
    errors = [r for r in results if r >= 500]
    print(f"       {len(results)} concurrent issues in {elapsed:.1f}s: {ok} posted, {refused} refused, {len(errors)} server errors")
    check(ok == 50 and not errors, "exactly the available stock was issued, no server errors")
    s, _, b = call("GET", "/api/reports/stock_balance?item=" + item["id"])
    qty = sum(float(r["qty"]) for r in b["rows"])
    check(s == 200 and qty == 0, "stock balance is exactly zero, never negative")
    s, _, b = call("GET", "/api/reports/trial_balance")
    check(s == 200 and b["meta"]["balanced"], "trial balance still balances")
    print("smoke test passed")


if __name__ == "__main__":
    main()
