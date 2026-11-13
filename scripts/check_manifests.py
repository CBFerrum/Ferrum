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


def rendered(name, text):
    """The documents of one stream of YAML, such as `helm template` prints."""
    return [(name, doc) for doc in yaml.safe_load_all(text) if doc]


def pool_size(spec):
    env = [e for c in spec["template"]["spec"]["containers"] for e in c.get("env", [])]
    if not any(e["name"] == "DATABASE_URL" for e in env):
        return None
    return next((int(e["value"]) for e in env if e["name"] == "FERRUM_DB_MAX_CONNECTIONS"), 30)


def through_pooler(spec):
    return any(e["name"] == "FERRUM_DATABASE_DIRECT_URL" for c in spec["template"]["spec"]["containers"] for e in c.get("env", []))


def per_replica(spec, pooler):
    """What one replica opens on PostgreSQL itself: its pool and side connections, or, when it reaches the
    database through the pooler (it then has FERRUM_DATABASE_DIRECT_URL), only its direct ones."""
    size = pool_size(spec)
    if size is None:
        return None
    if pooler and through_pooler(spec):
        return int(pooler["metadata"].get("annotations", {}).get("ferrum.dev/direct-connections-per-replica", 8))
    return size + SIDE


def pooler_connections(pooler):
    """A PgBouncer pooler's connections to PostgreSQL: each instance's pool and reserve (PgBouncer's defaults
    20 and 0)."""
    if not pooler:
        return 0
    params = pooler["spec"].get("pgbouncer", {}).get("parameters", {})
    each = int(params.get("default_pool_size", 20)) + int(params.get("reserve_pool_size", 0))
    return pooler["spec"].get("instances", 1) * each


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
    # The read-write pooler in front of the cluster, if there is one.
    pooler = next((d for _, d in docs if d.get("kind") == "Pooler" and d["spec"].get("type", "rw") == "rw"
                   and clusters and d["spec"]["cluster"]["name"] == clusters[0]["metadata"]["name"]), None)
    for hpa in scalers:
        target = hpa["spec"]["scaleTargetRef"]
        entry = workloads.get((target["kind"], target["name"]))
        if entry is None or not clusters:
            continue
        each = per_replica(entry[1]["spec"], pooler)
        if each is None:
            continue
        through = pooler is not None and through_pooler(entry[1]["spec"])
        needed = hpa["spec"]["maxReplicas"] * each + OTHERS + (pooler_connections(pooler) if through else 0)
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

    # Every workload on the role at once: each server Deployment at its most replicas (its autoscaler's
    # maximum, or its replicas), and each Job that runs the server (a migration).
    if not clusters:
        return found
    roles = clusters[0]["spec"].get("managed", {}).get("roles", [])
    limit = next((r.get("connectionLimit", -1) for r in roles if r["name"] == "ferrum"), -1)
    most = {(h["spec"]["scaleTargetRef"]["kind"], h["spec"]["scaleTargetRef"]["name"]): h["spec"]["maxReplicas"] for h in scalers}
    parts = []
    for _, d in docs:
        if d.get("kind") in ("Deployment", "StatefulSet", "Job"):
            each = per_replica(d["spec"], pooler)
            if each is not None:
                key = (d["kind"], d["metadata"]["name"])
                n = most.get(key, d["spec"].get("parallelism" if d["kind"] == "Job" else "replicas", 1))
                parts.append((key, n * each))
    if pooler:
        parts.append((("Pooler", pooler["metadata"]["name"]), pooler_connections(pooler)))
    needed = sum(n for _, n in parts) + OTHERS
    alone = len(parts) == 1 and parts[0][0] in most  # already said above
    if limit >= 0 and needed > limit and not alone:
        names = ", ".join(f"{k[1]} {n}" for k, n in parts)
        found.append(f"all together ({names}, others {OTHERS}) they need {needed} connections, and the role may hold {limit}")
    return found


if __name__ == "__main__":
    # A directory of manifests (deploy/k8s by default), or a file of rendered documents, or - for stdin:
    #   helm template ferrum deploy/helm/ferrum -f values.yaml | python3 scripts/check_manifests.py -
    source = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "deploy" / "k8s")
    if source == "-":
        docs = rendered("stdin", sys.stdin.read())
    elif pathlib.Path(source).is_dir():
        docs = documents(pathlib.Path(source))
    else:
        docs = rendered(pathlib.Path(source).name, pathlib.Path(source).read_text())
    wrong = problems(docs)
    for line in wrong:
        print(line)
    sys.exit(1 if wrong else 0)
