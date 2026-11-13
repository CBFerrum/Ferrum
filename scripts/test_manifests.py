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
assert any("no connection limit" in p for p in changed("Cluster", lambda d: d["spec"].pop("managed")))
assert any("leaving less" in p for p in changed("Cluster", lambda d: d["spec"]["managed"]["roles"][0].update(connectionLimit=399)))

# A rendered chart with pools: two server Deployments, one without an autoscaler, and a migration Job.
# Each alone fits; together they do not.
def server(kind, name, pool, **spec):
    env = [{"name": "DATABASE_URL", "value": "x"}, {"name": "FERRUM_DB_MAX_CONNECTIONS", "value": str(pool)}]
    template = {"spec": {"containers": [{"name": "ferrum", "env": env}]}}
    return ("chart", {"kind": kind, "metadata": {"name": name}, "spec": dict(spec, template=template)})


cluster = [("chart", d) for _, d in real if d.get("kind") == "Cluster"]
hpa = ("chart", {"kind": "HorizontalPodAutoscaler", "metadata": {"name": "ferrum"},
                 "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "ferrum"}, "maxReplicas": 8}})
fits = cluster + [hpa, server("Deployment", "ferrum", 30), server("Deployment", "ferrum-worker", 20, replicas=2)]
assert problems(fits) == [], problems(fits)  # 8 x 34 + 2 x 24 + 20 = 340
over = fits + [server("Job", "ferrum-migrate", 30)]  # + 34 = 374
assert [p for p in problems(over) if "all together" in p and "374" in p], problems(over)
assert problems(fits + [server("Deployment", "ferrum-more", 30, replicas=1)]) != []  # no autoscaler: still counted
# Workloads that do not use the database are not counted.
pdf = ("chart", {"kind": "Deployment", "metadata": {"name": "pdf"}, "spec": {"replicas": 50, "template": {"spec": {"containers": [{"name": "p"}]}}}})
assert problems(fits + [pdf]) == []
assert rendered("x", "a: 1\n---\n---\nb: 2\n") == [("x", {"a": 1}), ("x", {"b": 2})]
print("the manifests say what they mean")
