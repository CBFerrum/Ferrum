#!/usr/bin/env python3
"""What the Kubernetes manifests promise about scaling, checked before anything is applied.

Two things here go wrong quietly. A Deployment that sets replicas under an autoscaler goes back to that
number on every apply, which is how a deployment during a peak cuts capacity. And a number of replicas that
fits the database's connections on the day it was chosen stops fitting when either side changes.
"""
import pathlib
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Each replica's short side writes (SIDE_CONNECTIONS in crates/kernel/src/db.rs).
SIDE = 4
# What else uses this role: Cloud's controller at two replicas of ten, when it is deployed.
OTHERS = 20
# What PostgreSQL must keep for an administrator and replication (RESERVED_CONNECTIONS in doctor.rs).
RESERVED = 20


def documents(directory):
    for path in sorted(directory.glob("*.yaml")):
        for doc in yaml.safe_load_all(path.read_text()):
            if doc:
                yield path.name, doc


def problems(docs):
    docs = list(docs)
    found = []
    workloads = {(d["kind"], d["metadata"]["name"]): (f, d) for f, d in docs if d.get("kind") in ("Deployment", "StatefulSet")}
    scalers = [d for _, d in docs if d.get("kind") == "HorizontalPodAutoscaler"]
    for hpa in scalers:
        target = hpa["spec"]["scaleTargetRef"]
        entry = workloads.get((target["kind"], target["name"]))
        if entry is None:
            found.append(f"{hpa['metadata']['name']}: scales {target['kind']}/{target['name']}, which is not here")
        elif "replicas" in entry[1]["spec"]:
            found.append(f"{entry[0]}: {target['name']} sets replicas under an autoscaler, and two things now own that number")

    clusters = [d for _, d in docs if d.get("kind") == "Cluster"]
    for hpa in scalers:
        target = hpa["spec"]["scaleTargetRef"]
        entry = workloads.get((target["kind"], target["name"]))
        if entry is None or not clusters:
            continue
        env = [e for c in entry[1]["spec"]["template"]["spec"]["containers"] for e in c.get("env", [])]
        pool = next((int(e["value"]) for e in env if e["name"] == "FERRUM_DB_MAX_CONNECTIONS"), 30)
        needed = hpa["spec"]["maxReplicas"] * (pool + SIDE) + OTHERS
        spec = clusters[0]["spec"]
        allowed = int(spec["postgresql"]["parameters"]["max_connections"])
        roles = spec.get("managed", {}).get("roles", [])
        limit = next((r.get("connectionLimit", -1) for r in roles if r["name"] == "ferrum"), -1)
        if limit < 0:
            found.append("database: the ferrum role has no connection limit, so nothing keeps a reserve")
        elif limit > allowed - RESERVED:
            found.append(f"database: the ferrum role may hold {limit} of {allowed}, leaving less than {RESERVED}")
        if limit >= 0 and needed > limit:
            found.append(f"{target['name']}: at {hpa['spec']['maxReplicas']} replicas it needs {needed} connections, and the role may hold {limit}")
    return found


if __name__ == "__main__":
    directory = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "deploy" / "k8s"
    wrong = problems(documents(directory))
    for line in wrong:
        print(line)
    sys.exit(1 if wrong else 0)
