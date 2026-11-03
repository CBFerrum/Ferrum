#!/usr/bin/env python3
"""AT-CLOUD-003 against a running server: a health-only support key, tried on every way in (stdlib only).

    FERRUM_URL=http://127.0.0.1:5150 FERRUM_TENANT=acme FERRUM_EMAIL=admin@acme.test FERRUM_PASSWORD=... \
        python3 scripts/support_check.py

Makes a grant, tries it, reads what the customer sees of that, and ends it. Run it against a staging or demo workspace.
"""

import http.client
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

BASE = os.environ.get("FERRUM_URL", "http://127.0.0.1:5150").rstrip("/")
TENANT = os.environ.get("FERRUM_TENANT", "demo")
EMAIL = os.environ.get("FERRUM_EMAIL", "admin@demo.test")
PASSWORD = os.environ.get("FERRUM_PASSWORD", "Correct-Horse-9")


def call(method, path, token=None, body=None):
    req = urllib.request.Request(BASE + path, data=None if body is None else json.dumps(body).encode(), method=method)
    req.add_header("content-type", "application/json")
    if token:
        req.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw.decode(errors="replace")


def socket_status(token):
    """What a WebSocket upgrade is answered with. urllib would insist on `Connection: close`."""
    url = urllib.parse.urlsplit(BASE)
    c = http.client.HTTPConnection(url.hostname, url.port, timeout=30)
    upgrade = {"Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ=="}
    c.request("GET", "/ws/live", headers={"Authorization": f"Bearer {token}", **upgrade})
    return c.getresponse().status


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        sys.exit(1)


def main():
    s, b = call("POST", "/api/auth/login", body={"tenant": TENANT, "email": EMAIL, "password": PASSWORD})
    check(s == 200, "the administrator signs in")
    admin = b["token"]
    s, g = call("POST", "/api/admin/support", admin, {"level": "health", "grantee": "Check", "reason": "support_check.py", "hours": 1})
    check(s in (200, 201) and g.get("key"), "a health-only grant")
    grant, key = g["id"], g["key"]

    s, _ = call("GET", "/api/support/report", key)
    check(s == 200, "it reaches what it was given")
    tried = [
        ("GET", "/api/v1/Customer"),  # records
        ("POST", "/api/v1/Customer"),
        ("GET", "/api/admin/support"),  # administration
        ("GET", "/api/reports/trial_balance"),  # reports
        ("GET", "/api/app/cloud/projects"),  # an app's own routes
        ("GET", "/api/auth/sessions"),  # what takes a browser session
        ("GET", "/api/assistant/usage"),
    ]
    for method, path in tried:
        s, _ = call(method, path, key, {} if method == "POST" else None)
        check(s in (401, 403, 404), f"{method} {path} is refused ({s})")

    check(socket_status(key) in (401, 403), "a live socket is refused")

    s, r = call("GET", f"/api/admin/support/{grant}/requests", admin)
    seen = {x["path"]: x["refused"] for x in r["requests"]} if s == 200 else {}
    check(seen.get("/api/support/report") is False, "the customer sees what it reached")
    check(seen.get("/api/v1/Customer") is True and seen.get("outside the API") is True, f"and what it tried and was refused: {sorted(seen)}")

    s, _ = call("DELETE", f"/api/admin/support/{grant}", admin)
    check(s in (200, 204), "the customer ends it")
    s, _ = call("GET", "/api/support/report", key)
    check(s == 401, "and it has ended")
    print("support access check passed")


if __name__ == "__main__":
    main()
