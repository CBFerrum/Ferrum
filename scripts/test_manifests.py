#!/usr/bin/env python3
"""scripts/check_manifests.py against what it exists to catch, and against the real manifests."""
import copy
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from check_manifests import ROOT, documents, problems

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
print("the manifests say what they mean")
