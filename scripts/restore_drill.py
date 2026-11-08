#!/usr/bin/env python3
"""A restore drill (FLC-OPS-004, FLC-OPS-005; the package's AT-OPS-005): the original infrastructure is taken
away and the platform recovered into a new, isolated environment, with PostgreSQL and Temporal's histories
taken at different moments. PostgreSQL is backed up before an order was made; Temporal's histories after.

What must hold, and is checked:
- the restored server notices it is not on the database it knew, and holds work that reaches outside;
- runs and their histories read back with the same keys, so nothing is lost or unreadable;
- an effect asked for while held waits; it is done once, after a person releases the hold;
- a run whose history says it did something this database has no receipt for is named for a person to
  check, and is not done again by itself.

    scripts/restore_drill.py

Settings come from the environment, defaulting to the owner's walk setup (see the constants below). The
original server and Temporal must be running; the drill stops them, and starts them again at the end.
"""

import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = [sys.argv[0], os.environ.get("DRILL_BASE", "http://127.0.0.1:5170"), os.environ.get("DRILL_TENANT", "acme"), "globex"]
import lrp_walk as w  # noqa: E402

ERP = os.path.expanduser("~/Desktop/Personal/ERP/flc")
PG = os.environ.get("DRILL_PG", "postgres://ferrum:ferrum@localhost:5432")
SOURCE = os.environ.get("DRILL_SOURCE_DB", "ferrum_flc_walk")
TARGET = os.environ.get("DRILL_TARGET_DB", "ferrum_flc_drill")
FERRUM = os.environ.get("DRILL_FERRUM", f"{ERP}/target/debug/ferrum")
TEMPORAL = os.environ.get("DRILL_TEMPORAL", f"{ERP}/bin/temporal")
TEMPORAL_DB = os.environ["DRILL_TEMPORAL_DB"]
WORK = os.environ["DRILL_WORK"]
PACKAGES = os.environ.get("DRILL_PACKAGES", f"{ERP}/walk/packages")
UI = os.environ.get("DRILL_UI", f"{ERP}/web/dist-apps")
STOP_ORIGINAL = os.environ.get("DRILL_STOP_ORIGINAL", "pkill -f 'flc/target/debug/ferrum start'; pkill -f 'temporal server start-dev'")
START_ORIGINAL = os.environ.get("DRILL_START_ORIGINAL", "")
NEW_PORT, NEW_TEMPORAL = 5171, 7234
SID = "workspace.lrp_procurement"


def sh(cmd, **kw):
    print(f"     $ {cmd if isinstance(cmd, str) else ' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, shell=isinstance(cmd, str), check=True, **kw)


def wait_port(port, seconds=90):
    import socket
    until = time.time() + seconds
    while time.time() < until:
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(1)
    return False


def held(base):
    import urllib.request
    text = urllib.request.urlopen(base + "/metrics", timeout=10).read().decode()
    return next((line.split()[-1] for line in text.splitlines() if line.startswith("ferrum_restore_held ")), "?")


def main():
    os.makedirs(WORK, exist_ok=True)
    print(f"== Before: on the original environment ({w.BASE})")
    p = {"admin": w.Person(w.HOME, f"admin@{w.HOME}.test"), "requester": w.Person(w.HOME, f"requester@{w.HOME}.test"),
         "reviewer": w.Person(w.HOME, f"reviewer@{w.HOME}.test")}
    reqs, runs = [], []
    for i in (1, 2):
        s, req = w.create(p["requester"], "Request", {"reference": f"DRILL-{w.RUN}-{i}", "total": "900"})
        p["requester"]("POST", f"/api/v1/Request/{req['id']}/submit", {})
        run = w.run_of(p["admin"], SID, req["id"])
        w.task_of(p["admin"], run["id"], "review")
        reqs.append(req["id"])
        runs.append(run["id"])
    w.check("two runs wait for their reviews", len(runs) == 2, runs)
    dump = f"{WORK}/{SOURCE}-t1.dump"
    sh(["pg_dump", "-Fc", "-f", dump, f"{PG}/{SOURCE}"])
    print("     PostgreSQL backed up (t1)")
    task1 = w.task_of(p["admin"], runs[0], "review")
    w.decide(p["reviewer"], task1["id"])
    first = w.eventually(lambda: w.rows(p["requester"]("GET", f"/api/v1/Order?filters=%5B%5B%22request%22%2C%22%3D%22%2C%22{reqs[0]}%22%5D%5D")[1]), 60)
    w.check("after the backup, the first run ordered (PostgreSQL's backup does not know it; Temporal's history will)", bool(first), first)
    print("== The original infrastructure goes away")
    subprocess.run(STOP_ORIGINAL, shell=True)
    time.sleep(3)
    copy = f"{WORK}/temporal-t2.db"
    for suffix in ("", "-wal", "-shm"):
        if os.path.exists(TEMPORAL_DB + suffix):
            shutil.copy2(TEMPORAL_DB + suffix, copy + suffix)
    print("     Temporal's store copied (t2), after the order")

    print(f"== A new, isolated environment: database {TARGET}, Temporal on {NEW_TEMPORAL}, Ferrum on {NEW_PORT}")
    sh(["dropdb", "--if-exists", f"--maintenance-db={PG}/postgres", TARGET])
    sh(["createdb", f"--maintenance-db={PG}/postgres", TARGET])
    sh(["pg_restore", "--no-owner", "-d", f"{PG}/{TARGET}", dump])
    log = open(f"{WORK}/drill-temporal.log", "w")
    temporal = subprocess.Popen([TEMPORAL, "server", "start-dev", "--headless", "--port", str(NEW_TEMPORAL), "--namespace", "ferrum-flc", "--log-level", "warn", "--db-filename", copy],
                                stdout=log, stderr=log)
    w.check("Temporal starts from the copied store", wait_port(NEW_TEMPORAL), None)
    env = dict(os.environ, DATABASE_URL=f"{PG}/{TARGET}", PORT=str(NEW_PORT), FERRUM_PACKAGES_DIR=PACKAGES, FERRUM_UI_DIR=UI,
               TEMPORAL_ADDRESS=f"http://127.0.0.1:{NEW_TEMPORAL}", TEMPORAL_NAMESPACE="ferrum-flc")
    flog = open(f"{WORK}/drill-ferrum.log", "w")
    server = subprocess.Popen([FERRUM, "start"], env=env, stdout=flog, stderr=flog, cwd=f"{ERP}/ferrum/server")
    try:
        w.check("Ferrum starts on the restored database", wait_port(NEW_PORT, 120), None)
        time.sleep(3)
        w.BASE = f"http://127.0.0.1:{NEW_PORT}"
        n = {"admin": w.Person(w.HOME, f"admin@{w.HOME}.test"), "requester": w.Person(w.HOME, f"requester@{w.HOME}.test"),
             "reviewer": w.Person(w.HOME, f"reviewer@{w.HOME}.test")}
        w.check("it notices it is not on the database it knew, and holds", held(w.BASE) == "1", held(w.BASE))
        seen = n["admin"]("GET", f"{w.STUDIO}/runs")[1]["runs"]
        w.check("both runs are there, read with the same keys", all(any(r["id"] == x for r in seen) for x in runs), [r["id"] for r in seen][:5])
        task2 = w.task_of(n["admin"], runs[1], "review")
        w.check("the second run's review is still open, as the backup had it", bool(task2), runs[1])
        w.decide(n["reviewer"], task2["id"])
        time.sleep(8)
        q = "/api/v1/Order?filters=%5B%5B%22request%22%2C%22%3D%22%2C%22{}%22%5D%5D"
        w.check("while held, the order asked for waits", w.rows(n["requester"]("GET", q.format(reqs[1]))[1]) == [], None)
        w.check("the first run's order is not in the restored database", w.rows(n["requester"]("GET", q.format(reqs[0]))[1]) == [], None)
        sh(f"DATABASE_URL={PG}/{TARGET} FERRUM_PACKAGES_DIR={PACKAGES} TEMPORAL_ADDRESS=http://127.0.0.1:{NEW_TEMPORAL} TEMPORAL_NAMESPACE=ferrum-flc "
           f"{FERRUM} task release_restore note:'Drill: checked the restored database against the order book' > {WORK}/release.log 2>&1", cwd=f"{ERP}/ferrum/server")
        w.check("a person releases the hold", held(w.BASE) == "0", held(w.BASE))
        second = w.eventually(lambda: w.rows(n["requester"]("GET", q.format(reqs[1]))[1]), 90)
        w.check("released, the held order is made", len(second or []) == 1, second)
        time.sleep(5)
        w.check("and only once", len(w.rows(n["requester"]("GET", q.format(reqs[1]))[1])) == 1, None)
        named = w.task_of(n["admin"], runs[0], "order#restored")
        w.check("the first run, whose history says it ordered, is named for a person: no receipt here", bool(named), runs[0])
        w.check("and nothing ordered again for it by itself", w.rows(n["requester"]("GET", q.format(reqs[0]))[1]) == [], None)
    finally:
        server.terminate()
        temporal.terminate()
        server.wait(30)
        temporal.wait(30)
        if START_ORIGINAL:
            subprocess.run(START_ORIGINAL, shell=True)
    print(f"\n{len(w.FAILED)} failed" if w.FAILED else "\nthe drill held")
    sys.exit(1 if w.FAILED else 0)


if __name__ == "__main__":
    main()
