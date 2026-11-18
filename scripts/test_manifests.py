#!/usr/bin/env python3
"""scripts/check_manifests.py against what it exists to catch: two owners of one replica count, and the chart's figures
for the connections a replica opens drifting from the kernel's. (The connection budget itself is the chart's template's;
deploy/helm/test_pools.sh proves its refusals.)"""
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import check_manifests
from check_manifests import ROOT, problems, rendered

# What the chart renders by default: an autoscaler, and a Deployment that leaves replicas to it.
chart = subprocess.run(
    ["helm", "template", "ferrum", str(ROOT / "deploy/helm/ferrum"), "--set", "image.tag=abc", "--set", "publicUrl=https://erp.acme.test",
     "--set", "storage.bucket=acme-files", "--set", "gateway.hostname=erp.acme.test", "--set", "office.host=office.acme.test",
     "--set", "database.urlSecret.name=db", "--set", "database.connectionLimit=460", "--set", "database.backupReachDays=30"],
    check=True, capture_output=True, text=True,
).stdout
real = rendered("chart", chart)
assert problems(real) == [], problems(real)
assert check_manifests.chart_drift() == [], check_manifests.chart_drift()


def changed(kind, change):
    import copy

    docs = copy.deepcopy(real)
    for _, d in docs:
        if d.get("kind") == kind:
            change(d)
    return problems(docs)


assert any("two things now own" in p for p in changed("Deployment", lambda d: d["spec"].update(replicas=3) if d["metadata"]["name"] == "ferrum" else None))


# A KEDA ScaledObject is an autoscaler too: never beside another on one Deployment.
def scaled(name, most):
    return ("chart", {"kind": "ScaledObject", "metadata": {"name": name}, "spec": {"scaleTargetRef": {"name": name}, "maxReplicaCount": most}})


def server(name, **spec):
    return ("chart", {"kind": "Deployment", "metadata": {"name": name}, "spec": dict(spec, template={"spec": {"containers": [{"name": "ferrum"}]}})})


both = ("chart", {"kind": "HorizontalPodAutoscaler", "metadata": {"name": "w"},
                  "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "ferrum-worker"}, "maxReplicas": 2}})
assert problems([server("ferrum-worker"), scaled("ferrum-worker", 1)]) == []
assert any("two things now own" in p for p in problems([server("ferrum-worker", replicas=2), scaled("ferrum-worker", 2)]))
assert any("both scale" in p for p in problems([server("ferrum-worker"), both, scaled("ferrum-worker", 2)]))
assert rendered("x", "a: 1\n---\n---\nb: 2\n") == [("x", {"a": 1}), ("x", {"b": 2})]

# The figures the chart counts with must be what the kernel opens: a chart that says otherwise is reported.
text = (ROOT / "deploy/helm/ferrum/templates/_pools.tpl").read_text()
for old, new in (('{{- define "ferrum.detached" -}}2{{- end -}}', '{{- define "ferrum.detached" -}}3{{- end -}}'),
                 ('{{- define "ferrum.direct" -}}2{{- end -}}', '{{- define "ferrum.direct" -}}9{{- end -}}'),
                 ("(div (add (int .) 7) 8)) 2 }}", "(div (add (int .) 7) 8)) 5 }}")):
    assert old in text, old
    assert check_manifests.chart_drift(text.replace(old, new)), f"{new} was not reported"
print("the manifests say what they mean")
