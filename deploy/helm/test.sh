#!/usr/bin/env bash
# The chart against deploy/k8s: lint, the image rule, and a resource-by-resource comparison, with
# the defaults and with the optional components on. Needs helm, kubectl and python3 with pyyaml;
# no cluster, no network. Run from the repo root: bash deploy/helm/test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
chart=deploy/helm/ferrum
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
digest=sha256:0000000000000000000000000000000000000000000000000000000000000000

printf 'image:\n  digest: %s\n' "$digest" > "$tmp/digest.yaml"
printf 'image:\n  tag: abc\neditors:\n  enabled: true\n  image: {tag: abc}\nmetricsEngine:\n  enabled: true\n  image: {tag: abc}\n' > "$tmp/all.yaml"
# The Namespace is rendered here because deploy/k8s has it; with Helm, --create-namespace does it.
tpl() { helm template ferrum "$chart" --namespace ferrum --set namespace.create=true "$@"; }

echo "lint"
helm lint "$chart" -f "$tmp/digest.yaml"

echo "no digest and no tag fails"
if err=$(helm template ferrum "$chart" 2>&1); then echo "rendered without an image version"; exit 1; fi
grep -q 'no default image version' <<<"$err" || { echo "unexpected error: $err"; exit 1; }
if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" --set editors.enabled=true 2>&1); then echo "editors rendered without an image version"; exit 1; fi
grep -q 'editors.image' <<<"$err" || { echo "unexpected error: $err"; exit 1; }

echo "a digest wins over a tag"
tpl -f "$tmp/digest.yaml" --set image.tag=abc | grep -q "image: ghcr.io/cbferrum/ferrum@$digest"

# The online editor is a second kustomization (its own namespace): the two are compared with the chart together.
kust() { { kubectl kustomize "$1"; echo ---; kubectl kustomize "$1/office"; } > "$2"; }

echo "defaults against deploy/k8s and deploy/k8s/office"
kust deploy/k8s "$tmp/k.yaml"
tpl -f "$tmp/digest.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "editors and metrics engine against deploy/k8s with both components"
cp -R deploy/k8s "$tmp/k8s"
sed -i.bak -e 's/^# components:/components:/' -e 's/^#   - editors/  - editors/' -e 's/^#   - metrics-engine/  - metrics-engine/' "$tmp/k8s/kustomization.yaml"
grep -q '^  - metrics-engine' "$tmp/k8s/kustomization.yaml"
cp -R deploy/k8s/office "$tmp/k8s/office"
kust "$tmp/k8s" "$tmp/k.yaml"
tpl -f "$tmp/all.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "the online editor is on by default, and one switch takes it out of the chart and the server's settings"
tpl -f "$tmp/digest.yaml" > "$tmp/on.yaml"
grep -q 'FERRUM_OFFICE_URL' "$tmp/on.yaml" && grep -q 'name: ferrum-office' "$tmp/on.yaml" && grep -q 'name: office-https' "$tmp/on.yaml"
tpl -f "$tmp/digest.yaml" --set office.enabled=false > "$tmp/off.yaml"
! grep -qE 'ferrum-office|FERRUM_OFFICE|office-https' "$tmp/off.yaml" || exit 1
echo "no PodDisruptionBudget for one editor replica, one for two"
! grep -q 'name: ferrum-office' <(python3 - "$tmp/on.yaml" <<'PY'
import sys, yaml
for d in yaml.safe_load_all(open(sys.argv[1])):
    if d and d["kind"] == "PodDisruptionBudget": print("name:", d["metadata"]["name"])
PY
)
tpl -f "$tmp/digest.yaml" --set office.replicas=2 | grep -A3 'kind: PodDisruptionBudget' | grep -q 'name: ferrum-office'
echo "the editor accepts one WOPI host, has no console, and runs as a non-root user"
python3 - "$tmp/on.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum-office")
c = dep["spec"]["template"]["spec"]["containers"][0]
env = {e["name"]: e["value"] for e in c["env"]}
assert env["aliasgroup1"] == "http://ferrum.ferrum.svc.cluster.local:80", env
assert "*" not in env["aliasgroup1"], env
assert "admin_console.enable=false" in env["extra_params"] and "net.frame_ancestors=https://erp.example.com" in env["extra_params"], env
assert dep["spec"]["template"]["spec"]["securityContext"]["runAsNonRoot"] is True
assert c["securityContext"]["capabilities"]["drop"] == ["ALL"]
assert c["resources"]["limits"]["memory"] and c["resources"]["requests"]["memory"]
assert c["readinessProbe"] and c["livenessProbe"]
srv = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
senv = {e["name"]: e.get("value") for e in srv["spec"]["template"]["spec"]["containers"][0]["env"]}
assert senv["FERRUM_OFFICE_ORIGIN"] == "https://office.example.com", senv
PY

echo "migration job is a pre-install,pre-upgrade hook, and off by default"
! tpl -f "$tmp/digest.yaml" | grep -q 'kind: Job' || exit 1
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true --is-upgrade | grep -q 'helm.sh/hook: pre-install,pre-upgrade'
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true,database.enabled=false,database.urlSecret.name=db | grep -q 'helm.sh/hook: pre-install,pre-upgrade'

echo "on the first install with the chart's own database (not there yet when hooks run), the pods migrate themselves"
! tpl -f "$tmp/digest.yaml" --set migrate.enabled=true | grep -qE 'kind: Job|FERRUM_MIGRATE_AT_START' || exit 1

echo "enabling it runs the migrate task and stops the server from migrating itself"
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true --is-upgrade > "$tmp/m.yaml"
python3 - "$tmp/m.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
job = next(d for d in docs if d["kind"] == "Job")
assert job["spec"]["template"]["spec"]["containers"][0]["args"] == ["task", "migrate"], job
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
env = {e["name"]: e.get("value") for e in dep["spec"]["template"]["spec"]["containers"][0]["env"]}
assert env.get("FERRUM_MIGRATE_AT_START") == "false", env
PY
! tpl -f "$tmp/digest.yaml" | grep -q FERRUM_MIGRATE_AT_START || exit 1

echo "by default no settings Job, and the pods' environment as before"
! tpl -f "$tmp/digest.yaml" --is-upgrade | grep -qE 'ferrum-settings|application_name|FERRUM_DATABASE_ADDRESS' || exit 1

echo "a settings document alone: a Job applies it after the migrate Job, and the pods keep their environment"
tpl -f "$tmp/digest.yaml" --is-upgrade --set migrate.enabled=true --set-json 'settings={"installation":{"session_days":"7"},"others":"keep"}' > "$tmp/s.yaml"
python3 - "$tmp/s.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
jobs = {d["metadata"]["name"]: d for d in docs if d["kind"] == "Job"}
weight = lambda j: int(j["metadata"]["annotations"]["helm.sh/hook-weight"])
assert weight(jobs["ferrum-settings"]) > weight(jobs["ferrum-migrate"]), "after the migration"
c = jobs["ferrum-settings"]["spec"]["template"]["spec"]["containers"][0]
assert c["args"] == ["task", "apply_settings", "file:/etc/ferrum/settings/settings.yaml"], c["args"]
cm = next(d for d in docs if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "ferrum-settings")
assert "helm.sh/hook" in cm["metadata"]["annotations"], "there before the Job runs"
assert yaml.safe_load(cm["data"]["settings.yaml"]) == {"installation": {"session_days": "7"}, "others": "keep"}, cm["data"]
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
env = {e["name"] for e in dep["spec"]["template"]["spec"]["containers"][0]["env"]}
assert {"PUBLIC_URL", "FERRUM_S3_BUCKET", "LOG_LEVEL"} <= env, env
PY
! tpl -f "$tmp/digest.yaml" --set-json 'settings={"installation":{"session_days":"7"}}' | grep -q 'name: ferrum-settings' || exit 1

echo "environment: minimal: each pod has its database address naming its pool, the secrets and what apps read themselves"
tpl -f "$tmp/digest.yaml" --is-upgrade --set environment=minimal --set-json 'settings={"installation":{"session_days":"7","log_level":"warn"}}' > "$tmp/min.yaml"
# The preset fills the role's connection limit; the settings Job is counted in it too.
tpl -f "$tmp/digest.yaml" --is-upgrade --set environment=minimal -f "$chart/presets/pools.yaml" --set dbMaxConnections=16 > "$tmp/minp.yaml"
python3 - "$tmp/min.yaml" "$tmp/minp.yaml" <<'PY'
import sys, yaml
KEPT = {"FERRUM_DATABASE_ADDRESS", "FERRUM_DB_USER", "FERRUM_DB_PASSWORD", "FERRUM_DB_NAME", "FERRUM_DATABASE_DIRECT_URL",
        "FERRUM_DATABASE_READ_URL", "FERRUM_OFFICE_URL", "FERRUM_OFFICE_WOPI_URL", "FERRUM_EDITORS_URL", "FERRUM_METRICS_ENGINE_URL"}
def load(path):
    docs = [d for d in yaml.safe_load_all(open(path)) if d]
    servers = {d["metadata"]["name"]: d for d in docs if d["kind"] == "Deployment" and any(
        e["name"] == "DATABASE_URL" for e in d["spec"]["template"]["spec"]["containers"][0].get("env", []))}
    cm = next(d for d in docs if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "ferrum-settings")
    return docs, servers, yaml.safe_load(cm["data"]["settings.yaml"])
def check(dep, pool):
    env = dep["spec"]["template"]["spec"]["containers"][0]["env"]
    by = {e["name"]: e for e in env}
    assert by["DATABASE_URL"]["value"] == f"$(FERRUM_DATABASE_ADDRESS)?application_name=pool:{pool}", by["DATABASE_URL"]
    assert by["FERRUM_DATABASE_ADDRESS"]["valueFrom"]["secretKeyRef"] == {"name": "ferrum-db-app", "key": "uri"}
    plain = {e["name"] for e in env if "value" in e} - {"DATABASE_URL"} - KEPT
    assert not plain, f"{dep['metadata']['name']}: settings left in the environment: {plain}"
    names = [e["name"] for e in env]
    assert names.index("FERRUM_DATABASE_ADDRESS") < names.index("DATABASE_URL"), "defined before the address uses it"

docs, servers, doc = load(sys.argv[1])
check(servers["ferrum"], "default")
inst = doc["installation"]
assert inst["public_url"] == "https://erp.example.com" and inst["s3_bucket"] == "CHANGE-ME-files-bucket", inst
assert inst["session_days"] == "7" and inst["log_level"] == "warn", "the document's own over the chart's"
assert inst["pdf_url"].startswith("http://ferrum-pdf"), inst
assert not {"database_url", "jwt_secret", "secret_key", "office_url", "db_max_connections", "zone"} & set(inst), inst
assert doc["pools"] == {"default": {"db_max_connections": "30", "run_background": "true", "zone": ""}}, doc["pools"]
assert any(d["kind"] == "Job" and d["metadata"]["name"] == "ferrum-settings" for d in docs)

docs, servers, doc = load(sys.argv[2])
pools = {"ferrum": "general", "ferrum-public": "public", "ferrum-finance": "finance", "ferrum-worker": "worker"}
assert set(servers) == set(pools), set(servers)
for name, pool in pools.items():
    check(servers[name], pool)
assert doc["pools"]["general"] == {"db_max_connections": "16", "run_background": "false", "zone": ""}, doc["pools"]
assert doc["pools"]["worker"]["run_background"] == "true", doc["pools"]
PY
if err=$(tpl -f "$tmp/digest.yaml" --set environment=minimal 2>&1); then echo "rendered minimal on a first install with the chart's own database"; exit 1; fi
grep -q 'install with environment: full' <<<"$err" || { echo "unexpected error: $err"; exit 1; }
tpl -f "$tmp/digest.yaml" --set environment=minimal,database.enabled=false,database.urlSecret.name=db,database.urlHasQuery=true \
  | grep -q 'value: $(FERRUM_DATABASE_ADDRESS)&application_name=pool:default'

echo "an outside database: no Cluster, the address from its Secret, a way out to it, its certificate authority at the fixed path"
ext=(--set database.enabled=false --set database.external.urlSecret.name=db --set-json 'database.external.egress.cidrs=["10.20.0.0/16"]'
  --set database.external.caSecret.name=db-ca --set migrate.enabled=true --set-json 'settings={"installation":{"session_days":"7"}}' --is-upgrade)
tpl -f "$tmp/digest.yaml" "${ext[@]}" > "$tmp/ext.yaml"
tpl -f "$tmp/digest.yaml" "${ext[@]}" --set database.external.directHost=db-direct.internal:5432 --set database.external.readHost=db-ro.internal \
  --set-json 'database.external.egress.ports=[5432,6432]' > "$tmp/extp.yaml"
tpl -f "$tmp/digest.yaml" --set database.enabled=false --set database.urlSecret.name=old-db > "$tmp/exto.yaml"
python3 - "$tmp/ext.yaml" "$tmp/extp.yaml" "$tmp/exto.yaml" <<'PY'
import sys, yaml
def load(path):
    docs = [d for d in yaml.safe_load_all(open(path)) if d]
    assert not [d for d in docs if d["kind"] in ("Cluster", "Pooler", "ScheduledBackup", "ObjectStore")], "the chart runs no database"
    return docs
def pod(d):
    return d["spec"]["template"]["spec"]
def env(d):
    return {e["name"]: e for e in pod(d)["containers"][0]["env"]}
def workloads(docs):
    return [d for d in docs if d["kind"] in ("Deployment", "Job") and d["metadata"]["name"] in ("ferrum", "ferrum-migrate", "ferrum-settings")]

docs = load(sys.argv[1])
assert len(workloads(docs)) == 3, [d["metadata"]["name"] for d in workloads(docs)]
for w in workloads(docs):
    e = env(w)
    assert e["DATABASE_URL"]["valueFrom"]["secretKeyRef"] == {"name": "db", "key": "uri"}, w["metadata"]["name"]
    assert not {"FERRUM_DATABASE_DIRECT_HOST", "FERRUM_DATABASE_READ_HOST"} & set(e), "nothing direct without a host"
    mounts = {m["name"]: m for m in pod(w)["containers"][0]["volumeMounts"]}
    assert mounts["run"] == {"name": "run", "mountPath": "/run/ferrum", "readOnly": True}, mounts
    vols = {v["name"]: v for v in pod(w)["volumes"]}
    assert vols["run"]["projected"]["sources"] == [
        {"secret": {"name": "ferrum-vault-key", "items": [{"key": "root.key", "path": "root.key"}], "optional": True}},
        {"secret": {"name": "db-ca", "items": [{"key": "ca.crt", "path": "db-ca.pem"}]}},
    ], vols["run"]
policy = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "ferrum")
out = [r for r in policy["spec"]["egress"] if r.get("to") and "ipBlock" in r["to"][0]]
assert out == [{"to": [{"ipBlock": {"cidr": "10.20.0.0/16"}}], "ports": [{"port": 5432}]}], policy["spec"]["egress"]

docs = load(sys.argv[2])
for w in workloads(docs):
    e, name = env(w), w["metadata"]["name"]
    assert e["FERRUM_DATABASE_DIRECT_HOST"]["value"] == "db-direct.internal:5432", name
    assert ("FERRUM_DATABASE_READ_HOST" in e) == (name == "ferrum"), f"{name}: reads from the replica only where requests are served"
policy = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "ferrum")
assert {"to": [{"ipBlock": {"cidr": "10.20.0.0/16"}}], "ports": [{"port": 5432}, {"port": 6432}]} in policy["spec"]["egress"]

docs = load(sys.argv[3])
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
assert env(dep)["DATABASE_URL"]["valueFrom"]["secretKeyRef"] == {"name": "old-db", "key": "uri"}, "the older name still works"
run = {v["name"]: v for v in pod(dep)["volumes"]}["run"]
assert [s["secret"]["name"] for s in run["projected"]["sources"]] == ["ferrum-vault-key"], "no certificate authority: the key file alone"
PY
if err=$(tpl -f "$tmp/digest.yaml" --set database.enabled=false 2>&1); then echo "rendered an outside database without its address"; exit 1; fi
grep -q 'database.external.urlSecret.name' <<<"$err" || { echo "unexpected error: $err"; exit 1; }
echo "the chart's own database: none of the outside database's settings show"
! tpl -f "$tmp/digest.yaml" --set database.external.directHost=x --set database.external.caSecret.name=y --set-json 'database.external.egress.cidrs=["10.0.0.0/8"]' \
  | grep -qE 'DIRECT_HOST|db-ca.pem|ipBlock: \{cidr: 10.0.0.0/8\}' || exit 1

echo "the vault's key: a Secret made once and kept, mounted in every pod and Job; nothing of the server's Secret required"
tpl -f "$tmp/digest.yaml" --is-upgrade --set migrate.enabled=true --set-json 'settings={"installation":{"session_days":"7"}}' > "$tmp/key.yaml"
tpl -f "$tmp/digest.yaml" --is-upgrade --set migrate.enabled=true --set vault.keySecret.name=old-cluster-key \
  --set vault.kms.tokenSecret.name=bao-token --set vault.kms.caSecret.name=bao-ca --set vault.kms.serviceAccount=true > "$tmp/keyn.yaml"
python3 - "$tmp/key.yaml" "$tmp/keyn.yaml" <<'PY'
import base64, sys, yaml
def load(path):
    return [d for d in yaml.safe_load_all(open(path)) if d]
def workloads(docs):
    return [d for d in docs if d["kind"] in ("Deployment", "Job") and d["metadata"]["name"] in ("ferrum", "ferrum-migrate", "ferrum-settings")]
def run(w):
    spec = w["spec"]["template"]["spec"]
    assert {"name": "run", "mountPath": "/run/ferrum", "readOnly": True} in spec["containers"][0]["volumeMounts"], w["metadata"]["name"]
    return {s["secret"]["items"][0]["path"]: s["secret"] for s in {v["name"]: v for v in spec["volumes"]}["run"]["projected"]["sources"]}
docs = load(sys.argv[1])
key = next(d for d in docs if d["kind"] == "Secret" and d["metadata"]["name"] == "ferrum-vault-key")
assert key["metadata"]["annotations"]["helm.sh/resource-policy"] == "keep", "kept when the release is removed"
assert len(base64.b64decode(base64.b64decode(key["data"]["root.key"]))) == 32, "base64 of 32 random bytes, as /run/ferrum/root.key holds them"
assert len(workloads(docs)) == 3
for w in workloads(docs):
    sources = run(w)
    assert sources["root.key"] == {"name": "ferrum-vault-key", "items": [{"key": "root.key", "path": "root.key"}], "optional": True}, sources
    assert w["spec"]["template"]["spec"]["automountServiceAccountToken"] is False
    for e in w["spec"]["template"]["spec"]["containers"][0]["env"]:
        ref = e.get("valueFrom", {}).get("secretKeyRef")
        if ref and ref["name"] == "ferrum-secrets":
            assert ref.get("optional") is True, f"{e['name']} is not required any more"
docs = load(sys.argv[2])
assert not [d for d in docs if d["kind"] == "Secret" and d["metadata"]["name"] == "ferrum-vault-key"], "a named Secret is not made"
for w in workloads(docs):
    sources = run(w)
    assert sources["root.key"]["name"] == "old-cluster-key" and sources["kms.token"]["name"] == "bao-token" and sources["kms-ca.pem"]["name"] == "bao-ca", sources
    assert w["spec"]["template"]["spec"]["automountServiceAccountToken"] is True, "signs in to the key-management service as the pod"
PY
echo "a new cluster that takes another's database (import, copy, recovery) mounts the key named for it"
for mode in import copy recovery; do
  tpl -f "$tmp/digest.yaml" --is-upgrade --set database.bootstrap.mode=$mode --set database.bootstrap.source.host=old-db \
    --set database.bootstrap.source.user=ferrum --set database.bootstrap.source.passwordSecret.name=old-pw \
    --set database.backup.serverName=ferrum-db-2 --set vault.keySecret.name=old-cluster-key > "$tmp/$mode.yaml"
  python3 - "$tmp/$mode.yaml" "$mode" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
assert any(d["kind"] == "Cluster" for d in docs), sys.argv[2]
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
run = {v["name"]: v for v in dep["spec"]["template"]["spec"]["volumes"]}["run"]
assert run["projected"]["sources"][0]["secret"]["name"] == "old-cluster-key", (sys.argv[2], run)
PY
done
echo "environment: minimal with an outside database: the direct and read hosts are the address's own parameters"
tpl -f "$tmp/digest.yaml" --is-upgrade --set environment=minimal --set database.enabled=false --set database.external.urlSecret.name=db \
  --set database.external.directHost=db-direct.internal:5432 --set database.external.readHost=db-ro.internal > "$tmp/minh.yaml"
python3 - "$tmp/minh.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
env = {e["name"]: e for e in dep["spec"]["template"]["spec"]["containers"][0]["env"]}
assert env["DATABASE_URL"]["value"] == "$(FERRUM_DATABASE_ADDRESS)?application_name=pool:default&ferrum_direct_host=db-direct.internal:5432&ferrum_read_host=db-ro.internal", env["DATABASE_URL"]
assert not {"FERRUM_DATABASE_DIRECT_HOST", "FERRUM_DATABASE_READ_HOST"} & set(env), "nothing but the address"
cm = next(d for d in docs if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "ferrum-settings")
assert not {"database_direct_host", "database_read_host"} & set(yaml.safe_load(cm["data"]["settings.yaml"])["installation"]), "not settings in the table"
PY

echo "an ExternalSecret makes the server's Secret only when asked, and needs its store"
! tpl -f "$tmp/digest.yaml" | grep -q 'kind: ExternalSecret' || exit 1
tpl -f "$tmp/digest.yaml" --set secrets.externalSecret.enabled=true --set secrets.externalSecret.secretStoreRef.name=vault \
  --set-json 'secrets.externalSecret.dataFrom=[{"extract":{"key":"ferrum"}}]' | grep -A10 'kind: ExternalSecret' | grep -q 'name: ferrum-secrets'
if err=$(tpl -f "$tmp/digest.yaml" --set secrets.externalSecret.enabled=true 2>&1); then echo "rendered an ExternalSecret without a store"; exit 1; fi
grep -q 'secretStoreRef.name' <<<"$err"

echo "ok"
