#!/usr/bin/env bash
# The product chart: lint, the image rule, the values that have no default, what it renders for a database it is given
# the address of (no Cluster, no backups, no pooler) and for the optional components. Needs helm and python3 with
# pyyaml; no cluster, no network. Run from the repo root: bash deploy/helm/test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
chart=deploy/helm/ferrum
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
digest=sha256:0000000000000000000000000000000000000000000000000000000000000000

printf 'image:\n  digest: %s\n' "$digest" > "$tmp/digest.yaml"
# What ships as a placeholder is a site's own to set (the chart refuses it otherwise): one site's, for every render.
# The database's three values have no default either: the address's Secret, the role's connections (the default 9 replicas,
# a rollout and both Jobs fit 458) and the days the backups reach.
printf 'publicUrl: https://erp.acme.test\nstorage: {bucket: acme-files}\ngateway: {hostname: erp.acme.test}\nprivateGateway: {hostnames: [erp.internal.acme.test]}\noffice: {host: office.acme.test}\neditors: {host: editors.acme.test}\ndatabase: {urlSecret: {name: db}, connectionLimit: 460, backupReachDays: 30}\n' > "$tmp/site.yaml"
printf 'image:\n  tag: abc\neditors:\n  enabled: true\n  image: {tag: abc}\nmetricsEngine:\n  enabled: true\n  image: {tag: abc}\n' > "$tmp/all.yaml"
tpl() { helm template ferrum "$chart" --namespace ferrum "$@" -f "$tmp/site.yaml"; }

echo "lint"
helm lint "$chart" -f "$tmp/digest.yaml" -f "$tmp/site.yaml"

echo "the placeholders fail, naming each one still set"
if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" 2>&1); then echo "rendered with its placeholders"; exit 1; fi
for v in publicUrl storage.bucket gateway.hostname office.host; do
  grep -q "$v" <<<"$err" || { echo "the error does not name $v: $err"; exit 1; }
done
if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" -f "$tmp/site.yaml" --set editors.enabled=true --set editors.image.tag=abc \
  --set editors.host=editors.example.com --set privateGateway.enabled=true --set-json 'privateGateway.hostnames=["erp.internal.example.com"]' 2>&1); then
  echo "rendered with an enabled component's placeholder"; exit 1; fi
grep -q "editors.host" <<<"$err" && grep -q "privateGateway.hostnames" <<<"$err" || { echo "unexpected error: $err"; exit 1; }

echo "no digest and no tag fails"
if err=$(helm template ferrum "$chart" -f "$tmp/site.yaml" 2>&1); then echo "rendered without an image version"; exit 1; fi
grep -q 'no default image version' <<<"$err" || { echo "unexpected error: $err"; exit 1; }
if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" -f "$tmp/site.yaml" --set editors.enabled=true 2>&1); then echo "editors rendered without an image version"; exit 1; fi
grep -q 'editors.image' <<<"$err" || { echo "unexpected error: $err"; exit 1; }

echo "a digest wins over a tag"
tpl -f "$tmp/digest.yaml" --set image.tag=abc | grep -q "image: ghcr.io/cbferrum/ferrum@$digest"

echo "the optional components render with their own settings"
tpl -f "$tmp/all.yaml" > "$tmp/c.yaml"
python3 - "$tmp/c.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
names = {(d["kind"], d["metadata"]["name"]) for d in docs}
for want in [("Deployment", "ferrum-editors"), ("Deployment", "ferrum-metrics-engine"), ("Deployment", "ferrum-pdf"), ("Deployment", "ferrum-office")]:
    assert want in names, want
PY

echo "no database in the chart: no Cluster, Pooler, backup or CloudNativePG object, whatever is set"
! grep -qiE 'kind: (Cluster|Pooler|ScheduledBackup|ObjectStore)$|cnpg|barman' "$tmp/c.yaml" || exit 1

echo "the engine's files: its Secret by default and when asked; rendered with environment: minimal (the image declares its service) and when asked"
engine=(-f "$tmp/digest.yaml" --set metricsEngine.enabled=true --set metricsEngine.image.tag=abc
  --set-json 'database.egress.to=[{"podSelector":{"matchLabels":{"app":"pg"}}}]')
tpl "${engine[@]}" > "$tmp/e-default.yaml"
tpl "${engine[@]}" --set environment=minimal > "$tmp/e-minimal.yaml"
tpl "${engine[@]}" --set metricsEngine.files=rendered --set database.directHost=db-direct.internal:5432 \
  --set-json 'database.egress.to=[{"ipBlock":{"cidr":"10.20.0.0/16"}}]' > "$tmp/e-rendered.yaml"
grep -q 'list "metrics-engine"' "$chart/templates/_helpers.tpl"
tpl "${engine[@]}" --set environment=minimal --set metricsEngine.files=secret > "$tmp/e-kept.yaml"
python3 - "$tmp" <<'PY'
import sys, yaml
tmp = sys.argv[1]
def load(name):
    docs = [d for d in yaml.safe_load_all(open(f"{tmp}/{name}.yaml")) if d]
    by = {(d["kind"], d["metadata"]["name"]): d for d in docs}
    eng = by[("Deployment", "ferrum-metrics-engine")]["spec"]["template"]["spec"]
    srv = by[("Deployment", "ferrum")]["spec"]["template"]["spec"]["containers"][0]
    policy = by[("NetworkPolicy", "ferrum-metrics-engine")]["spec"]
    return eng, {e["name"]: e for e in eng["containers"][0]["env"]}, {e["name"] for e in srv["env"]}, policy
def secret(name):
    eng, env, server, policy = load(name)
    assert "initContainers" not in eng and policy["egress"] == [], name
    assert env["FERRUM_METRICS_ENGINE_TOKEN"]["valueFrom"]["secretKeyRef"] == {"name": "ferrum-metrics-engine", "key": "token"}, name
    assert "FERRUM_METRICS_ENGINE_TOKEN" in server and "FERRUM_METRICS_ENGINE_TOKEN_FILE" not in env, name
def rendered(name, egress):
    eng, env, server, policy = load(name)
    init = eng["initContainers"][0]
    assert init["args"] == ["task", "render", "service:metrics-engine", "to:/run/companion"], init["args"]
    assert {e["name"] for e in init["env"]} <= {"DATABASE_URL", "FERRUM_DATABASE_DIRECT_HOST"}, "the database's address is its one input"
    assert init["securityContext"]["readOnlyRootFilesystem"] is True and init["securityContext"]["capabilities"]["drop"] == ["ALL"]
    mounts = {m["name"]: m for m in init["volumeMounts"]}
    assert mounts["companion"] == {"name": "companion", "mountPath": "/run/companion"} and mounts["run"]["mountPath"] == "/run/ferrum", mounts
    vols = {v["name"]: v for v in eng["volumes"]}
    assert vols["companion"]["emptyDir"]["medium"] == "Memory", "in memory, never on a disk"
    assert {"name": "companion", "mountPath": "/run/companion", "readOnly": True} in eng["containers"][0]["volumeMounts"], "read-only where the engine runs"
    assert env["FERRUM_METRICS_ENGINE_TOKEN_FILE"]["value"] == "/run/companion/token" and "FERRUM_METRICS_ENGINE_TOKEN" not in env, env
    assert "FERRUM_METRICS_ENGINE_TOKEN" not in server, "the server reads it from Metrics' vault"
    assert eng["automountServiceAccountToken"] is False
    assert policy["egress"][0] == egress and {"port": 53, "protocol": "UDP"} in policy["egress"][1]["ports"], policy["egress"]
    return init
own_db = {"to": [{"podSelector": {"matchLabels": {"app": "pg"}}}], "ports": [{"port": 5432}]}
for name in ["e-default", "e-kept"]:
    secret(name)
init = rendered("e-rendered", {"to": [{"ipBlock": {"cidr": "10.20.0.0/16"}}], "ports": [{"port": 5432}]})
env = {e["name"]: e for e in init["env"]}
assert env["DATABASE_URL"]["valueFrom"]["secretKeyRef"] == {"name": "db", "key": "uri"} and env["FERRUM_DATABASE_DIRECT_HOST"]["value"] == "db-direct.internal:5432", env
rendered("e-minimal", own_db)
PY
if err=$(tpl "${engine[@]}" --set metricsEngine.files=typed 2>&1); then echo "rendered an engine whose files are neither secret nor rendered"; exit 1; fi
grep -q "metrics-engine's files are secret or rendered" <<<"$err" || { echo "unexpected error: $err"; exit 1; }

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
assert "admin_console.enable=false" in env["extra_params"] and "net.frame_ancestors=https://erp.acme.test" in env["extra_params"], env
assert dep["spec"]["template"]["spec"]["securityContext"]["runAsNonRoot"] is True
assert c["securityContext"]["capabilities"]["drop"] == ["ALL"]
assert c["resources"]["limits"]["memory"] and c["resources"]["requests"]["memory"]
assert c["readinessProbe"] and c["livenessProbe"]
srv = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
senv = {e["name"]: e.get("value") for e in srv["spec"]["template"]["spec"]["containers"][0]["env"]}
assert senv["FERRUM_OFFICE_ORIGIN"] == "https://office.acme.test", senv
PY

echo "the migrate Job is a pre-install,pre-upgrade hook, on by default (the database is there before the release), on a first install too"
tpl -f "$tmp/digest.yaml" | grep -q 'helm.sh/hook: pre-install,pre-upgrade'
tpl -f "$tmp/digest.yaml" --is-upgrade | grep -q 'helm.sh/hook: pre-install,pre-upgrade'
echo "off, no Job, and the pods migrate themselves"
! tpl -f "$tmp/digest.yaml" --set migrate.enabled=false | grep -qE 'kind: Job|FERRUM_MIGRATE_AT_START' || exit 1

echo "it runs the migrate task and stops the server from migrating itself"
tpl -f "$tmp/digest.yaml" > "$tmp/m.yaml"
python3 - "$tmp/m.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
job = next(d for d in docs if d["kind"] == "Job")
assert job["spec"]["template"]["spec"]["containers"][0]["args"] == ["task", "migrate"], job
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
env = {e["name"]: e.get("value") for e in dep["spec"]["template"]["spec"]["containers"][0]["env"]}
assert env.get("FERRUM_MIGRATE_AT_START") == "false", env
PY
! tpl -f "$tmp/digest.yaml" --set migrate.enabled=false | grep -q FERRUM_MIGRATE_AT_START || exit 1

echo "by default no settings Job, and the pods' environment as before"
! tpl -f "$tmp/digest.yaml" | grep -qE 'ferrum-settings|application_name|FERRUM_DATABASE_ADDRESS' || exit 1

echo "a settings document alone: a Job applies it after the migrate Job, and the pods keep their environment"
tpl -f "$tmp/digest.yaml" --set-json 'settings={"installation":{"session_days":"7"},"others":"keep"}' > "$tmp/s.yaml"
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
tpl -f "$tmp/digest.yaml" --set-json 'settings={"installation":{"session_days":"7"}}' | grep -q 'name: ferrum-settings'

echo "environment: minimal: each pod has its database address naming its pool, the secrets and what apps read themselves"
tpl -f "$tmp/digest.yaml" --set environment=minimal --set-json 'settings={"installation":{"session_days":"7","log_level":"warn"}}' > "$tmp/min.yaml"
# The preset fills the role's connection limit; the settings Job is counted in it too.
tpl -f "$tmp/digest.yaml" --set environment=minimal -f "$chart/presets/pools.yaml" --set dbMaxConnections=16 > "$tmp/minp.yaml"
python3 - "$tmp/min.yaml" "$tmp/minp.yaml" <<'PY'
import sys, yaml
# The migrate Job is on, so the pods are told not to migrate themselves (migrate.serverEnv).
KEPT = {"FERRUM_DATABASE_ADDRESS", "FERRUM_OFFICE_URL", "FERRUM_OFFICE_WOPI_URL", "FERRUM_EDITORS_URL", "FERRUM_METRICS_ENGINE_URL", "FERRUM_MIGRATE_AT_START"}
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
    assert by["FERRUM_DATABASE_ADDRESS"]["valueFrom"]["secretKeyRef"] == {"name": "db", "key": "uri"}
    plain = {e["name"] for e in env if "value" in e} - {"DATABASE_URL"} - KEPT
    assert not plain, f"{dep['metadata']['name']}: settings left in the environment: {plain}"
    names = [e["name"] for e in env]
    assert names.index("FERRUM_DATABASE_ADDRESS") < names.index("DATABASE_URL"), "defined before the address uses it"

docs, servers, doc = load(sys.argv[1])
check(servers["ferrum"], "default")
inst = doc["installation"]
assert inst["public_url"] == "https://erp.acme.test" and inst["s3_bucket"] == "acme-files", inst
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
tpl -f "$tmp/digest.yaml" --set environment=minimal,database.urlHasQuery=true \
  | grep -q 'value: $(FERRUM_DATABASE_ADDRESS)&application_name=pool:default'

echo "the database it is given: no Cluster, the address from its Secret, a way out to it, its certificate authority at the fixed path, the days its backups reach"
ext=(--set database.caSecret.name=db-ca --set-json 'database.egress.to=[{"ipBlock":{"cidr":"10.20.0.0/16"}}]' --set-json 'settings={"installation":{"session_days":"7"}}')
tpl -f "$tmp/digest.yaml" "${ext[@]}" > "$tmp/ext.yaml"
tpl -f "$tmp/digest.yaml" "${ext[@]}" --set database.directHost=db-direct.internal:5432 --set database.readHost=db-ro.internal \
  --set-json 'database.egress.ports=[5432,6432]' > "$tmp/extp.yaml"
tpl -f "$tmp/digest.yaml" --set database.urlSecret.name=other-db > "$tmp/exto.yaml"
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
    assert w["metadata"]["name"] != "ferrum-migrate" or e["FERRUM_BACKUP_REACH_DAYS"]["value"] == "30"
    if w["kind"] == "Deployment":
        assert e["FERRUM_BACKUP_REACH_DAYS"]["value"] == "30" and e["FERRUM_FILES_DELETE_AFTER_DAYS"]["value"] == "30", "the days the backups reach, and a deletion waits"
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
assert env(dep)["DATABASE_URL"]["valueFrom"]["secretKeyRef"] == {"name": "other-db", "key": "uri"}, "the Secret named, under its key"
run = {v["name"]: v for v in pod(dep)["volumes"]}["run"]
assert [s["secret"]["name"] for s in run["projected"]["sources"]] == ["ferrum-vault-key"], "no certificate authority: the key file alone"
PY
echo "the values with no default fail naming each, and a value that is gone says what to set instead"
nodb=$(mktemp)
sed -e 's/^database: .*$/database: {}/' "$tmp/site.yaml" > "$nodb"
refuses() {
  local want=$1; shift
  if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" "$@" 2>&1); then echo "rendered, but should have refused: $want"; exit 1; fi
  grep -qF "$want" <<<"$err" || { echo "refused for another reason than \"$want\": $err"; exit 1; }
}
refuses "database.urlSecret.name is required" -f "$nodb" --set database.connectionLimit=460 --set database.backupReachDays=30
refuses "database.connectionLimit is required" -f "$nodb" --set database.urlSecret.name=db --set database.backupReachDays=30
refuses "database.backupReachDays is required" -f "$nodb" --set database.urlSecret.name=db --set database.connectionLimit=460
refuses "database.backupReachDays is a whole number" -f "$tmp/site.yaml" --set database.backupReachDays=thirty
for gone in enabled=true external.urlSecret.name=db instances=3 backup.retention=30d pooler.enabled=true readReplicas.enabled=true bootstrap.mode=import; do
  refuses "is gone:" -f "$tmp/site.yaml" --set "database.$gone"
done
refuses "database.enabled is gone: the chart no longer runs a database" -f "$tmp/site.yaml" --set database.enabled=true
refuses "set database.backupReachDays" -f "$tmp/site.yaml" --set database.backup.retention=30d
refuses "set database.directHost" -f "$tmp/site.yaml" --set database.pooler.enabled=true
rm -f "$nodb"
echo "no backups to reach is said: 0 days, and a deleted file then goes at once"
tpl -f "$tmp/digest.yaml" --set database.backupReachDays=0 | grep -A1 'FERRUM_BACKUP_REACH_DAYS' | grep -q 'value: "0"'

echo "a store in this cluster: its pods are a peer of the server's way out, on the port it gives"
tpl -f "$tmp/digest.yaml" --set-json 'storage.egress.to=[{"podSelector":{"matchLabels":{"app.kubernetes.io/name":"ferrum-store"}}}]' --set-json 'storage.egress.ports=[8333]' > "$tmp/store.yaml"
python3 - "$tmp/store.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
policy = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "ferrum")
assert {"to": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "ferrum-store"}}}], "ports": [{"port": 8333}]} in policy["spec"]["egress"], policy["spec"]["egress"]
PY
! tpl -f "$tmp/digest.yaml" | grep -q 'The object store (storage.egress)' || exit 1

echo "the vault's key: a Secret made once and kept, mounted in every pod and Job; nothing of the server's Secret required"
tpl -f "$tmp/digest.yaml" --set-json 'settings={"installation":{"session_days":"7"}}' > "$tmp/key.yaml"
tpl -f "$tmp/digest.yaml" --set vault.keySecret.name=old-cluster-key \
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
hook = {d["metadata"]["name"]: d["metadata"]["annotations"] for d in docs if d["kind"] == "Job"}
assert key["metadata"]["annotations"]["helm.sh/hook"] == "pre-install,pre-upgrade" and key["metadata"]["annotations"]["helm.sh/hook-weight"] == "-1", "there before the migrate Job of a first install"
assert "helm.sh/hook-delete-policy" not in key["metadata"]["annotations"], "no delete policy of its own"
assert int(hook["ferrum-migrate"]["helm.sh/hook-weight"]) > -1, "the migrate Job runs after it"
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
echo "environment: minimal with a pooler in front: the direct and read hosts are the address's own parameters"
tpl -f "$tmp/digest.yaml" --set environment=minimal --set database.directHost=db-direct.internal:5432 --set database.readHost=db-ro.internal > "$tmp/minh.yaml"
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
