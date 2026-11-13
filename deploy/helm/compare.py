#!/usr/bin/env python3
"""compare.py KUSTOMIZE.yaml CHART.yaml: fail when the chart's output and deploy/k8s drift apart.

Same kinds and names on both sides, and the same spec for each (images aside: the chart
requires a digest or tag, the base a placeholder to set). For the server, the environment variable
names are checked first so a setting missing from one side is named in the message.
"""
import sys
import yaml


def load(path):
    docs = [d for d in yaml.safe_load_all(open(path)) if d]
    return {(d["kind"], d["metadata"]["name"]): d for d in docs}


def strip_images(x):
    if isinstance(x, dict):
        # env is a set: kustomize puts a component's variables first, the chart last.
        return {k: sorted(strip_images(v), key=lambda e: e["name"]) if k == "env" else strip_images(v)
                for k, v in x.items() if k != "image"}
    if isinstance(x, list):
        return [strip_images(v) for v in x]
    return x


def containers(d):
    return d["spec"]["template"]["spec"]["containers"]


kust, chart = load(sys.argv[1]), load(sys.argv[2])
bad = []
for k in sorted(kust.keys() - chart.keys()):
    bad.append(f"{k[0]}/{k[1]}: in deploy/k8s, missing from the chart")
for k in sorted(chart.keys() - kust.keys()):
    bad.append(f"{k[0]}/{k[1]}: in the chart, not in deploy/k8s")
for k in sorted(kust.keys() & chart.keys()):
    a, b = kust[k], chart[k]
    if k[0] == "Deployment":
        for ca, cb in zip(containers(a), containers(b)):
            na = [e["name"] for e in ca.get("env", [])]
            nb = [e["name"] for e in cb.get("env", [])]
            if set(na) != set(nb):
                bad.append(f"{k[1]}: env only in deploy/k8s {sorted(set(na) - set(nb))}, only in the chart {sorted(set(nb) - set(na))}")
    sa, sb = strip_images(a.get("spec")), strip_images(b.get("spec"))
    if sa != sb:
        for f in sorted(set(sa or {}) | set(sb or {})):
            if (sa or {}).get(f) != (sb or {}).get(f):
                bad.append(f"{k[0]}/{k[1]}: spec.{f} differs\n    deploy/k8s: {str((sa or {}).get(f))[:600]}\n    chart:      {str((sb or {}).get(f))[:600]}")
if bad:
    print("\n".join(bad))
    sys.exit(1)
print(f"  {len(kust)} resources, same kinds, names and specs")
