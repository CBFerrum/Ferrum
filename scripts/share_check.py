#!/usr/bin/env python3
"""AT-SCALE-007 against a running server: one workspace floods it while another's reads are timed (stdlib only).

    FERRUM_URL=http://127.0.0.1:5150 FERRUM_FLOOD=acme:admin@acme.test FERRUM_OTHER=bravo:admin@bravo.test \
        FERRUM_PASSWORD=... python3 scripts/share_check.py

Both workspaces need the contacts package, and the server no per-user limit (`FERRUM_USER_REQUESTS_PER_MINUTE=0`), or
that answers the flood first. Each workspace gets a few hundred contacts. Then the other workspace's reads are timed
alone, and again while the first sends all it can from many threads. It says both ways the other's median and 95th
percentile, how long the flood waited for its turns (each response says, in `x-ferrum-waited-ms`), and how many the
server refused. Run it against a server of its own: the flood is real. `FERRUM_SHARE_EXPECTED=off` checks a server
started with `FERRUM_TENANT_REQUESTS=0` instead, to see the difference.
"""

import http.client
import json
import os
import statistics
import sys
import threading
import time
import urllib.parse

BASE = os.environ.get("FERRUM_URL", "http://127.0.0.1:5150").rstrip("/")
FLOOD = os.environ.get("FERRUM_FLOOD", "acme:admin@acme.test")
OTHER = os.environ.get("FERRUM_OTHER", "bravo:admin@bravo.test")
PASSWORD = os.environ.get("FERRUM_PASSWORD", "Correct-Horse-9")
THREADS = int(os.environ.get("FERRUM_FLOOD_THREADS", "64"))
SHARED = os.environ.get("FERRUM_SHARE_EXPECTED", "on") == "on"
READ = "/api/v1/Contact?limit=100"
URL = urllib.parse.urlsplit(BASE)


def call(c, method, path, token=None, body=None):
    headers = {"content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    c.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
    r = c.getresponse()
    raw = r.read()
    said = json.loads(raw) if raw and (r.getheader("content-type") or "").startswith("application/json") else raw
    return r.status, said, int(r.getheader("x-ferrum-waited-ms") or 0)


def connection():
    return http.client.HTTPConnection(URL.hostname, URL.port, timeout=60)


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        sys.exit(1)


def sign_in(who):
    tenant, email = who.split(":", 1)
    s, b, _ = call(connection(), "POST", "/api/auth/login", body={"tenant": tenant, "email": email, "password": PASSWORD})
    check(s == 200, f"{email} signs in to {tenant}")
    return b["token"]


def seed(token, n=300):
    c = connection()
    s, _, _ = call(c, "GET", "/api/v1/Contact?limit=1", token)
    check(s == 200, "the contacts package is there")
    for i in range(n):
        s, _, _ = call(c, "POST", "/api/v1/Contact", token, {"first_name": f"Share {i}", "last_name": "Check"})
        if s not in (200, 201):
            check(False, f"a contact is made ({s})")


def timed(token, n=200):
    """Each read's seconds, and how many of them waited for a turn."""
    c, took, waited = connection(), [], 0
    for _ in range(n):
        t = time.perf_counter()
        s, _, w = call(c, "GET", READ, token)
        took.append(time.perf_counter() - t)
        if s != 200:
            check(False, f"the other workspace's read is answered ({s})")
        waited += w > 0
    return took, waited


def flood(token, stop, out):
    c = connection()
    while not stop.is_set():
        try:
            out.append(call(c, "GET", READ, token)[::2])
        except (OSError, http.client.HTTPException):
            c = connection()


def percentiles(took):
    q = statistics.quantiles(took, n=20)
    return statistics.median(took), q[18]


def refused():
    _, body, _ = call(connection(), "GET", "/metrics")
    text = body.decode() if isinstance(body, bytes) else str(body)
    return next((int(float(line.split()[1])) for line in text.splitlines() if line.startswith("ferrum_tenant_refused_total ")), None)


def main():
    flooding, other = sign_in(FLOOD), sign_in(OTHER)
    seed(flooding)
    seed(other)
    alone, _ = timed(other)
    before = refused()
    stop, answers = threading.Event(), []
    threads = [threading.Thread(target=flood, args=(flooding, stop, answers), daemon=True) for _ in range(THREADS)]
    started = time.perf_counter()
    for t in threads:
        t.start()
    time.sleep(1)
    during, other_waited = timed(other, 500)
    stop.set()
    for t in threads:
        t.join(timeout=70)
    seconds = time.perf_counter() - started
    after = refused()

    (a50, a95), (d50, d95) = percentiles(alone), percentiles(during)
    statuses = {}
    for s, _ in answers:
        statuses[s] = statuses.get(s, 0) + 1
    waits = [w for _, w in answers if w > 0]
    print(f"  the other workspace alone: median {a50 * 1000:.1f} ms, 95th {a95 * 1000:.1f} ms")
    print(f"  while {THREADS} threads flood: median {d50 * 1000:.1f} ms, 95th {d95 * 1000:.1f} ms")
    print(f"  the flood: {len(answers)} requests in {seconds:.1f} s, answered {statuses}")
    if waits:
        print(f"  {len(waits)} of them waited for a turn, median {statistics.median(waits)} ms, longest {max(waits)} ms")
    print(f"  refused for their workspace's share: {None if before is None or after is None else after - before}")
    check(other_waited == 0, "the other workspace never waited for a turn")
    if SHARED:
        check(len(waits) > 0, "the flood waited for its own turns, and each answer said so")
        check(d95 < max(4 * a95, a95 + 0.25), "the other workspace's 95th percentile stays near what it was alone")
    else:
        check(not waits, "with no share, nothing waits for a turn")


if __name__ == "__main__":
    main()
