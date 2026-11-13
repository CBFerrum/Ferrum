#!/usr/bin/env python3
"""scripts/check_manifests.py against what it exists to catch, and against the real manifests."""
import copy
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from check_manifests import ROOT, documents, problems, rendered

real = list(documents(ROOT / "deploy" / "k8s"))
assert problems(real) == [], problems(real)


def changed(kind, change):
    docs = copy.deepcopy(real)
    for _, d in docs:
        if d.get("kind") == kind:
            change(d)
    return problems(docs)


assert any("two things now own" in p for p in changed("Deployment", lambda d: d["spec"].update(replicas=3)))
assert any("needs" in p for p in changed("HorizontalPodAutoscaler", lambda d: d["spec"].update(maxReplicas=12)))
# Each replica opens its pool and 6 more (4 side, 2 of Loco's own), and a rollout runs maxSurge more: 9 + 1 of them
# and Cloud's 20 need 380, all the role may hold. One more in a rollout does not fit, nor 25% of 9 rounded up (3).
assert any("needs 416" in p for p in changed("Deployment", lambda d: d["spec"]["strategy"]["rollingUpdate"].update(maxSurge=2)))
assert any("needs 452" in p for p in changed("Deployment", lambda d: d["spec"]["strategy"]["rollingUpdate"].update(maxSurge="25%")))
assert problems(changed("Deployment", lambda d: d["spec"].update(strategy={"type": "Recreate"}))) == []
assert any("no connection limit" in p for p in changed("Cluster", lambda d: d["spec"].pop("managed")))
assert any("leaving less" in p for p in changed("Cluster", lambda d: d["spec"]["managed"]["roles"][0].update(connectionLimit=399)))

# A rendered chart with pools: two server Deployments, one without an autoscaler, and a migration Job.
# Each alone fits; together they do not.
def server(kind, name, pool, **spec):
    env = [{"name": "DATABASE_URL", "value": "x"}, {"name": "FERRUM_DB_MAX_CONNECTIONS", "value": str(pool)}]
    template = {"spec": {"containers": [{"name": "ferrum", "env": env}]}}
    if kind == "Deployment":  # as the chart renders them
        spec.setdefault("strategy", {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1, "maxUnavailable": 0}})
    return ("chart", {"kind": kind, "metadata": {"name": name}, "spec": dict(spec, template=template)})


cluster = [("chart", d) for _, d in real if d.get("kind") == "Cluster"]
hpa = ("chart", {"kind": "HorizontalPodAutoscaler", "metadata": {"name": "ferrum"},
                 "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "ferrum"}, "maxReplicas": 7}})
fits = cluster + [hpa, server("Deployment", "ferrum", 30), server("Deployment", "ferrum-worker", 20, replicas=1)]
assert problems(fits) == [], problems(fits)  # (7 + 1) x 36 + (1 + 1) x 26 + 20 = 360
over = fits + [server("Job", "ferrum-migrate", 30)]  # + 36 (a Job runs no more in a rollout) = 396
assert [p for p in problems(over) if "all together" in p and "396" in p], problems(over)
assert problems(fits + [server("Deployment", "ferrum-more", 30, replicas=1)]) != []  # no autoscaler: still counted
# Workloads that do not use the database are not counted.
pdf = ("chart", {"kind": "Deployment", "metadata": {"name": "pdf"}, "spec": {"replicas": 50, "template": {"spec": {"containers": [{"name": "p"}]}}}})
assert problems(fits + [pdf]) == []
assert rendered("x", "a: 1\n---\n---\nb: 2\n") == [("x", {"a": 1}), ("x", {"b": 2})]

# With a pooler: a replica that reaches the database through it counts only its direct connections, and the
# pooler its own. 30 replicas x 36 would never fit; (30 + 1) x 8 + 2 x 40 + 20 = 348 does.
def through(name, pool, **spec):
    kind, d = server("Deployment", name, pool, **spec)
    d["spec"]["template"]["spec"]["containers"][0]["env"].append({"name": "FERRUM_DATABASE_DIRECT_URL", "value": "y"})
    return kind, d


pooler = ("chart", {"kind": "Pooler", "metadata": {"name": "ferrum-db-pooler", "annotations": {"ferrum.dev/direct-connections-per-replica": "8"}},
                    "spec": {"cluster": {"name": cluster[0][1]["metadata"]["name"]}, "type": "rw", "instances": 2,
                             "pgbouncer": {"parameters": {"default_pool_size": "40"}}}})
big = ("chart", {"kind": "HorizontalPodAutoscaler", "metadata": {"name": "ferrum"},
                 "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "ferrum"}, "maxReplicas": 30}})
pooled = cluster + [pooler, big, through("ferrum", 30)]
assert problems(pooled) == [], problems(pooled)
# The same replicas without the pooler do not fit.
assert any("needs" in p for p in problems(cluster + [big, server("Deployment", "ferrum", 30)]))
# A bigger pooler is counted: 31 x 8 + 2 x (40 + 50) + 20 = 448.
wide = copy.deepcopy(pooler)
wide[1]["spec"]["pgbouncer"]["parameters"]["reserve_pool_size"] = "50"
assert [p for p in problems(cluster + [wide, big, through("ferrum", 30)]) if "448" in p], problems(cluster + [wide, big, through("ferrum", 30)])
# A replica that does not use the pooler still counts in full beside it: + (3 + 1) x 36 = 492.
assert [p for p in problems(pooled + [server("Deployment", "ferrum-old", 30, replicas=3)]) if "all together" in p and "492" in p]

# A KEDA ScaledObject counts as an autoscaler: at its maxReplicaCount, never beside an HPA or fixed replicas.
def scaled(name, most):
    return ("chart", {"kind": "ScaledObject", "metadata": {"name": name}, "spec": {"scaleTargetRef": {"name": name}, "maxReplicaCount": most}})


worker = server("Deployment", "ferrum-worker", 20)
assert problems(cluster + [hpa, server("Deployment", "ferrum", 30), worker, scaled("ferrum-worker", 1)]) == []
# (7 + 1) x 36 + (5 + 1) x 26 + 20 = 464
assert [p for p in problems(cluster + [hpa, server("Deployment", "ferrum", 30), worker, scaled("ferrum-worker", 5)]) if "464" in p]
assert any("two things now own" in p for p in problems(cluster + [server("Deployment", "ferrum-worker", 20, replicas=2), scaled("ferrum-worker", 2)]))
both = ("chart", {"kind": "HorizontalPodAutoscaler", "metadata": {"name": "w"}, "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "ferrum-worker"}, "maxReplicas": 2}})
assert any("both scale" in p for p in problems(cluster + [worker, both, scaled("ferrum-worker", 2)]))
print("the manifests say what they mean")
