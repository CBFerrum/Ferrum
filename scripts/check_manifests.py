#!/usr/bin/env python3
"""What the product chart promises about scaling, checked on what it renders.

Two things here go wrong quietly. A Deployment that sets replicas under an autoscaler goes back to that number on every
apply, which is how a deployment during a peak cuts capacity. And the chart's own figures for the connections a replica
opens stop being right when the kernel's change: the budget the chart refuses by (deploy/helm/ferrum/templates/_pools.tpl,
against database.connectionLimit) is the template's, and its refusals are proven by deploy/helm/test_pools.sh; here only
that its three figures are what crates/kernel/src/db.rs opens.

    helm template ferrum deploy/helm/ferrum -f values.yaml | python3 scripts/check_manifests.py -
"""
import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent


def constant(name):
    """A connection count the kernel states (crates/kernel/src/db.rs), read from the code, so the budget follows it."""
    found = re.search(rf"pub const {name}: u32 = (\d+);", (ROOT / "crates/kernel/src/db.rs").read_text())
    return int(found.group(1))


# The direct pool's most: the one connection that LISTENs on every channel and a session lock at a time.
DIRECT = constant("LISTENERS") + constant("DIRECT_LOCKS")
# What a pod holds on connections outside every pool while it starts.
DETACHED = constant("DETACHED_LOCKS")


def chart_drift(pools=None):
    """Where the chart counts other connections than the code opens: its ferrum.beside, ferrum.detached and ferrum.direct
    (in `pools`, the text of _pools.tpl, or the file's)."""
    pools = pools or (ROOT / "deploy/helm/ferrum/templates/_pools.tpl").read_text()
    wrong = []
    line = f"{{{{ add (max {constant('SIDE_CONNECTIONS')} (div (add (int .) 7) 8)) {DIRECT} }}}}"
    if line not in pools:
        wrong.append(f"deploy/helm/ferrum/templates/_pools.tpl: ferrum.beside must be {line}, what crates/kernel/src/db.rs opens")
    for name, value, what in (("ferrum.detached", DETACHED, "holds"), ("ferrum.direct", DIRECT, "opens")):
        define = f'{{{{- define "{name}" -}}}}{value}{{{{- end -}}}}'
        if define not in pools:
            wrong.append(f"deploy/helm/ferrum/templates/_pools.tpl: {name} must be {define}, what crates/kernel/src/db.rs {what}")
    return wrong


def rendered(name, text):
    """The documents of one stream of YAML, such as `helm template` prints."""
    return [(name, doc) for doc in yaml.safe_load_all(text) if doc]


def problems(docs):
    docs = list(docs)
    found = []
    workloads = {(d["kind"], d["metadata"]["name"]): (f, d) for f, d in docs if d.get("kind") in ("Deployment", "StatefulSet")}
    scalers = [d for _, d in docs if d.get("kind") == "HorizontalPodAutoscaler"]
    # A KEDA ScaledObject makes an autoscaler of its own: never beside another on the same workload.
    for so in (d for _, d in docs if d.get("kind") == "ScaledObject"):
        ref = so["spec"]["scaleTargetRef"]
        target = {"kind": ref.get("kind", "Deployment"), "name": ref["name"]}
        if any(h["spec"]["scaleTargetRef"]["kind"] == target["kind"] and h["spec"]["scaleTargetRef"]["name"] == target["name"] for h in scalers):
            found.append(f"{so['metadata']['name']}: a ScaledObject and another autoscaler both scale {target['kind']}/{target['name']}")
            continue
        scalers.append({"metadata": so["metadata"], "spec": {"scaleTargetRef": target, "maxReplicas": so["spec"].get("maxReplicaCount", 100)}})
    for hpa in scalers:
        target = hpa["spec"]["scaleTargetRef"]
        entry = workloads.get((target["kind"], target["name"]))
        if entry is None:
            found.append(f"{hpa['metadata']['name']}: scales {target['kind']}/{target['name']}, which is not here")
        elif "replicas" in entry[1]["spec"]:
            found.append(f"{entry[0]}: {target['name']} sets replicas under an autoscaler, and two things now own that number")
    return found


if __name__ == "__main__":
    # A file of rendered documents, or - (the default) for stdin.
    source = sys.argv[1] if len(sys.argv) > 1 else "-"
    if source == "-":
        docs = rendered("stdin", sys.stdin.read())
    else:
        docs = rendered(pathlib.Path(source).name, pathlib.Path(source).read_text())
    wrong = chart_drift() + problems(docs)
    for line in wrong:
        print(line)
    sys.exit(1 if wrong else 0)
