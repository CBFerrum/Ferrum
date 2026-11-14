#!/usr/bin/env bash
# The chart's pools, zones and gateways (deploy/helm/ferrum/presets, docs/operations.md "Deployment shapes"):
# every preset renders and keeps its promises, and every refusal refuses. Needs helm and python3 with
# pyyaml; no cluster, no network. Run from the repo root: bash deploy/helm/test_pools.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
chart=deploy/helm/ferrum
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
tpl() { helm template ferrum "$chart" --namespace ferrum --set image.tag=abc "$@"; }

for preset in single split pools zones; do
  echo "$preset renders, fits the database and keeps its promises"
  tpl -f "$chart/presets/$preset.yaml" > "$tmp/$preset.yaml"
  python3 scripts/check_manifests.py "$tmp/$preset.yaml"
  python3 - "$tmp/$preset.yaml" "$preset" <<'PY'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
preset = sys.argv[2]
NAME = "app.kubernetes.io/name"

def env(dep):
    return {e["name"]: e.get("value") for e in dep["spec"]["template"]["spec"]["containers"][0].get("env", [])}

def matches(selector, labels):
    for k, v in selector.get("matchLabels", {}).items():
        if labels.get(k) != v:
            return False
    for e in selector.get("matchExpressions", []):
        assert e["operator"] == "In", e
        if labels.get(e["key"]) not in e["values"]:
            return False
    return True

deps = [d for d in docs if d["kind"] == "Deployment" and d["metadata"].get("namespace") is None]
servers = {d["metadata"]["name"]: d for d in deps if "DATABASE_URL" in env(d)}
assert "ferrum" in servers, "the first pool keeps today's name"
assert servers["ferrum"]["spec"]["selector"] == {"matchLabels": {NAME: "ferrum"}}, servers["ferrum"]["spec"]["selector"]

# No two Deployments' selectors match one pod: each pod template is selected by its own Deployment alone.
for d in deps:
    labels = d["spec"]["template"]["metadata"]["labels"]
    owners = [o["metadata"]["name"] for o in deps if matches(o["spec"]["selector"], labels)]
    assert owners == [d["metadata"]["name"]], (d["metadata"]["name"], owners)
    for key in ("ferrum.dev/pool", "ferrum.dev/zone"):
        assert key not in json.dumps(d["spec"]["selector"]), "pool and zone labels are never in a selector"

# Every Service that selects a server pod selects exactly one pool.
pool_of = {}  # Service name -> pool Deployment
for s in docs:
    if s["kind"] != "Service" or s["metadata"].get("namespace"):
        continue
    hit = [n for n, d in servers.items() if matches({"matchLabels": s["spec"]["selector"]}, d["spec"]["template"]["metadata"]["labels"])]
    assert len(hit) <= 1, (s["metadata"]["name"], hit)
    if hit:
        pool_of[s["metadata"]["name"]] = servers[hit[0]]

# Which gateways send traffic to which pool.
routed = {}  # pool Deployment name -> {gateway name}
for r in docs:
    if r["kind"] != "HTTPRoute" or r["metadata"].get("namespace"):
        continue
    gateways = {p["name"] for p in r["spec"]["parentRefs"]}
    for rule in r["spec"]["rules"]:
        for b in rule.get("backendRefs", []):
            assert b["name"] in pool_of, f"{r['metadata']['name']} sends to {b['name']}, which is no pool's Service"
            routed.setdefault(pool_of[b["name"]]["metadata"]["name"], set()).update(gateways)

zones = json.loads(env(servers["ferrum"]).get("FERRUM_ZONES") or "{}")
for name, d in servers.items():
    labels = d["spec"]["template"]["metadata"]["labels"]
    background = env(d).get("FERRUM_RUN_BACKGROUND", "true")
    zone = labels.get("ferrum.dev/zone")
    if zone and zones[zone].get("staff") == "none":
        assert background == "false", f"{name} is in {zone}, which admits no staff, and runs background work"
        assert env(d)["FERRUM_ZONE"] == zone
    has_service = any(v is d for v in pool_of.values())
    if preset != "single" and labels.get("ferrum.dev/pool") == "worker":
        assert not has_service and name not in routed, f"{name} runs only background work, yet has a Service or a route"
    # Its network policy admits only the gateways that route to it (and monitoring, and the editor for the first pool).
    if preset != "single":
        pol = next(p for p in docs if p["kind"] == "NetworkPolicy" and p["metadata"]["name"] == name)
        admitted = set()
        for src in pol["spec"]["ingress"][0]["from"]:
            pod = src.get("podSelector", {}).get("matchLabels", {})
            if "gateway.envoyproxy.io/owning-gateway-name" in pod:
                admitted.add(pod["gateway.envoyproxy.io/owning-gateway-name"])
            else:
                ns = src["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"]
                assert ns == "monitoring" or (ns == "ferrum-office" and name == "ferrum"), (name, src)
        assert admitted == routed.get(name, set()), (name, admitted, routed.get(name))

if preset == "zones":
    for name, gateways in routed.items():
        if "ferrum" in gateways:
            zone = servers[name]["spec"]["template"]["metadata"]["labels"]["ferrum.dev/zone"]
            assert zone == "public", f"the public gateway reaches {name}, in zone {zone}"
    assert routed["ferrum"] == {"ferrum-private"}, routed
    assert any(d["kind"] == "Gateway" and d["metadata"]["name"] == "ferrum-private" for d in docs)
if preset == "single":
    assert list(servers) == ["ferrum"]
PY
done

echo "each refusal refuses, saying why"
refuses() {
  local want=$1; shift
  if err=$(tpl "$@" 2>&1); then echo "rendered, but should have refused: $want"; exit 1; fi
  grep -qF "$want" <<<"$err" || { echo "refused for another reason than \"$want\": $err"; exit 1; }
}
small=(--set autoscaling.maxReplicas=2)
refuses "two pools are named web" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"web"}]'
refuses "two pools are named web" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b"},{"name":"web"}]'
refuses "is the name of another part of the chart" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"pdf"}]'
refuses "its zone pub admits no staff" "${small[@]}" --set-json 'zones={"pub":{"staff":"none"}}' --set-json 'pools=[{"name":"web"},{"name":"b","zone":"pub"}]'
refuses "its zone pub admits no staff" "${small[@]}" --set-json 'zones={"pub":{"staff":"none"}}' --set-json 'pools=[{"name":"web"},{"name":"b","zone":"pub","background":"only"}]'
refuses 'zone "nope" is not in zones' "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","zone":"nope"}]'
refuses 'names the gateway "private", which is not enabled' "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","routes":[{"gateway":"private","apps":["x"]}]}]'
refuses 'defaultPool "nope" is not a pool' "${small[@]}" --set gateway.defaultPool=nope --set-json 'pools=[{"name":"web"}]'
refuses "more than database.connectionLimit 380" --set-json 'pools=[{"name":"web"},{"name":"b"}]'
# (2 + 1) x 36 twice + 20 = 236 fits (one more pod each in a rollout); the migrate Job's 36 more, with a limit of
# 240, does not.
tpl "${small[@]}" --set database.connectionLimit=240 --set-json 'pools=[{"name":"web"},{"name":"b"}]' > /dev/null
refuses "the migrate Job 30 + 6" "${small[@]}" --set database.connectionLimit=240 --set migrate.enabled=true --set-json 'pools=[{"name":"web"},{"name":"b"}]' --is-upgrade
refuses "privateGateway.defaultPool: with pools, name the pool" "${small[@]}" --set privateGateway.enabled=true --set-json 'pools=[{"name":"web"},{"name":"b"}]'
tpl --set privateGateway.enabled=true > /dev/null  # without pools, the one pool answers it
refuses "cannot be background: only" --set-json 'pools=[{"name":"web","background":"only"}]'
refuses "has no routes" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","background":"only","routes":[{"apps":["x"]}]}]'
refuses "both take all of host h.example.com" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","routes":[{"hostnames":["h.example.com"]}]},{"name":"c","routes":[{"hostnames":["h.example.com"]}]}]'

echo "the database starts empty by default, or from another server or a backup, and refuses what a mode lacks"
src=(--set database.bootstrap.source.host=old.example.com --set database.bootstrap.source.passwordSecret.name=old-db)
for mode in import copy recovery; do
  extra=()
  case $mode in
    import) extra=("${src[@]}" --set database.bootstrap.source.user=postgres) ;;
    copy) extra=("${src[@]}" --set database.bootstrap.source.user=streaming_replica) ;;
    recovery) extra=(--set database.backup.serverName=ferrum-db-2 --set database.bootstrap.recovery.targetTime="2026-11-14 10:00:00+00") ;;
  esac
  tpl --set database.bootstrap.mode=$mode "${extra[@]}" > "$tmp/db-$mode.yaml"
done
tpl > "$tmp/db-empty.yaml"
python3 - "$tmp" <<'PY2'
import sys, yaml
def cluster(mode):
    docs = [d for d in yaml.safe_load_all(open(f"{sys.argv[1]}/db-{mode}.yaml")) if d]
    return next(d for d in docs if d["kind"] == "Cluster")["spec"]
assert cluster("empty")["bootstrap"] == {"initdb": {"database": "ferrum", "owner": "ferrum"}}
assert "externalClusters" not in cluster("empty")
c = cluster("import")
assert c["bootstrap"]["initdb"]["import"] == {"type": "microservice", "databases": ["ferrum"], "source": {"externalCluster": "origin"}}, c
assert c["externalClusters"][0]["connectionParameters"]["host"] == "old.example.com"
assert c["externalClusters"][0]["password"] == {"name": "old-db", "key": "password"}
c = cluster("copy")
assert c["bootstrap"] == {"pg_basebackup": {"source": "origin", "database": "ferrum", "owner": "ferrum"}}, c
assert c["externalClusters"][0]["connectionParameters"]["user"] == "streaming_replica"
c = cluster("recovery")
assert c["bootstrap"]["recovery"]["recoveryTarget"] == {"targetTime": "2026-11-14 10:00:00+00"}, c
assert c["externalClusters"][0]["plugin"]["parameters"] == {"barmanObjectName": "ferrum-backups", "serverName": "ferrum-db"}
# The recovered cluster writes its own backups under its new name, never over the ones it reads.
assert c["plugins"][0]["parameters"] == {"barmanObjectName": "ferrum-backups", "serverName": "ferrum-db-2"}, c["plugins"]
assert "serverName" not in cluster("empty")["plugins"][0]["parameters"]
PY2
refuses "set database.bootstrap.source.host" --set database.bootstrap.mode=import
refuses "a superuser: row-level security" --set database.bootstrap.mode=import --set database.bootstrap.source.host=h
refuses "REPLICATION and LOGIN" --set database.bootstrap.mode=copy --set database.bootstrap.source.host=h
refuses "source.passwordSecret.name" --set database.bootstrap.mode=copy --set database.bootstrap.source.host=h --set database.bootstrap.source.user=r
refuses "set database.backup.serverName to a new name" --set database.bootstrap.mode=recovery
refuses 'not "restore"' --set database.bootstrap.mode=restore
# Reading another cluster's backups needs no new name.
tpl --set database.bootstrap.mode=recovery --set database.bootstrap.recovery.serverName=old-db > /dev/null

echo "with the pooler, the server reaches the database through it and the migrate Job directly, within the budget"
tpl --set database.pooler.enabled=true --set migrate.enabled=true --is-upgrade -f "$chart/presets/split.yaml" > "$tmp/pooler.yaml"
python3 scripts/check_manifests.py "$tmp/pooler.yaml"
python3 - "$tmp/pooler.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
def env(d):
    return {e["name"]: e for e in d["spec"]["template"]["spec"]["containers"][0]["env"]}
for name in ("ferrum", "ferrum-worker"):
    dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)
    e = env(dep)
    assert "@ferrum-db-pooler.ferrum:5432/" in e["DATABASE_URL"]["value"], e["DATABASE_URL"]
    assert e["FERRUM_DATABASE_DIRECT_URL"]["valueFrom"]["secretKeyRef"] == {"name": "ferrum-db-app", "key": "uri"}
    names = list(e)
    assert names.index("FERRUM_DB_PASSWORD") < names.index("DATABASE_URL"), "a variable is used before it is set"
job = next(d for d in docs if d["kind"] == "Job")
e = env(job)
assert e["DATABASE_URL"]["valueFrom"]["secretKeyRef"] == {"name": "ferrum-db-app", "key": "uri"}, e["DATABASE_URL"]
assert "FERRUM_DATABASE_DIRECT_URL" not in e
p = next(d for d in docs if d["kind"] == "Pooler")
assert p["spec"]["cluster"] == {"name": "ferrum-db"} and p["spec"]["type"] == "rw"
assert p["spec"]["pgbouncer"]["poolMode"] == "transaction"
assert int(p["spec"]["pgbouncer"]["parameters"]["max_prepared_statements"]) > 0
pol = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "ferrum-db-pooler")
servers = pol["spec"]["ingress"][0]["from"][0]["podSelector"]["matchExpressions"][0]["values"]
assert servers == ["ferrum", "ferrum-worker"], servers
assert any(d["kind"] == "PodDisruptionBudget" and d["metadata"]["name"] == "ferrum-db-pooler" for d in docs)
PY2
# (6 + 1) x 8 + (2 + 1) x 8 + 2 x 40 + 36 + 20 = 216 fits 220; one more instance does not.
tpl --set database.pooler.enabled=true --set database.connectionLimit=220 -f "$chart/presets/split.yaml" --set migrate.enabled=true --is-upgrade > /dev/null
refuses "the pooler 3 x 40" --set database.pooler.enabled=true --set database.pooler.instances=3 --set database.connectionLimit=220 -f "$chart/presets/split.yaml" --set migrate.enabled=true --is-upgrade
refuses "the pooler 2 x 45" --set database.pooler.enabled=true --set-json 'database.pooler.parameters={"reserve_pool_size":5}' --set database.pooler.directConnectionsPerReplica=40
# Kept but not used: the server connects directly, and both count in the budget ((9 + 1) x 36 + 2 x 40 + 20 = 460).
refuses "the pooler 2 x 40, ferrum (9 + 1 in a rollout) x (30 + 6)" --set database.pooler.enabled=true --set database.pooler.used=false
tpl --set database.pooler.enabled=true --set database.pooler.used=false --set autoscaling.maxReplicas=6 > "$tmp/unused.yaml"
grep -q 'kind: Pooler' "$tmp/unused.yaml" && ! grep -q FERRUM_DATABASE_DIRECT_URL "$tmp/unused.yaml"
python3 scripts/check_manifests.py "$tmp/unused.yaml"
echo "every pool gets the database addresses through the shared env helper"
tpl -f "$chart/presets/zones.yaml" --set database.readReplicas.enabled=true --set database.pooler.enabled=true > "$tmp/zones-db.yaml"
python3 - "$tmp/zones-db.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
servers = [d for d in docs if d["kind"] == "Deployment" and any(e["name"] == "DATABASE_URL" for e in d["spec"]["template"]["spec"]["containers"][0].get("env", []))]
assert len(servers) == 4, [d["metadata"]["name"] for d in servers]
for d in servers:
    names = [e["name"] for e in d["spec"]["template"]["spec"]["containers"][0]["env"]]
    for want in ("FERRUM_DATABASE_DIRECT_URL", "FERRUM_DATABASE_READ_URL", "FERRUM_DATABASE_READ_MAX_LAG_SECS"):
        assert want in names, (d["metadata"]["name"], want)
    assert d["spec"]["strategy"]["rollingUpdate"]["maxSurge"] == 1, "the budget counts 1"
PY2
echo "the read address points the server at the standbys, never the migrate Job, and needs a standby"
tpl --set database.readReplicas.enabled=true --set migrate.enabled=true --set autoscaling.maxReplicas=8 --is-upgrade > "$tmp/ro.yaml"
python3 scripts/check_manifests.py "$tmp/ro.yaml"
python3 - "$tmp/ro.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
env = lambda d: {e["name"]: e for e in d["spec"]["template"]["spec"]["containers"][0]["env"]}
e = env(next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum"))
assert "@ferrum-db-ro.ferrum:5432/" in e["FERRUM_DATABASE_READ_URL"]["value"], e["FERRUM_DATABASE_READ_URL"]
assert e["FERRUM_DATABASE_READ_MAX_LAG_SECS"]["value"] == "10"
names = list(e)
assert names.index("FERRUM_DB_PASSWORD") < names.index("FERRUM_DATABASE_READ_URL"), "a variable is used before it is set"
assert "FERRUM_DATABASE_READ_URL" not in env(next(d for d in docs if d["kind"] == "Job"))
PY2
tpl --set database.readReplicas.enabled=true --set database.pooler.enabled=true | grep -c "name: FERRUM_DB_PASSWORD" | grep -qx 1
! tpl | grep -q FERRUM_DATABASE_READ_URL
refuses "database.readReplicas needs database.instances of 2 or more" --set database.readReplicas.enabled=true --set database.instances=1
refuses "needs database.enabled" --set database.pooler.enabled=true --set database.enabled=false --set database.urlSecret.name=x
refuses "is the name of another part of the chart" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"db-pooler"}]'

echo "a background pool can scale on waiting work with KEDA, never beside an autoscaler"
kedaon=(-f "$chart/presets/split.yaml" --set keda.prometheusAddress=http://prom:9090 --set-json 'pools[1]={"name":"worker","background":"only","autoscaling":{"enabled":false},"keda":{"enabled":true,"maxReplicas":2},"dbMaxConnections":20}')
tpl "${kedaon[@]}" > "$tmp/keda.yaml"
python3 scripts/check_manifests.py "$tmp/keda.yaml"
python3 - "$tmp/keda.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
so = [d for d in docs if d["kind"] == "ScaledObject"]
assert [d["metadata"]["name"] for d in so] == ["ferrum-worker"], so
spec = so[0]["spec"]
assert spec["scaleTargetRef"] == {"name": "ferrum-worker"} and spec["minReplicaCount"] == 1 and spec["maxReplicaCount"] == 2
for t in spec["triggers"]:
    # Every replica reports the same database-wide figure: the largest, never the sum.
    assert t["type"] == "prometheus" and t["metadata"]["query"].startswith("max(") and "sum(" not in t["metadata"]["query"], t
assert {t["metadata"]["query"].split("{")[0] for t in spec["triggers"]} == {"max(ferrum_outbox_pending", "max(ferrum_jobs_oldest_waiting_seconds"}
worker = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum-worker")
assert "replicas" not in worker["spec"], "KEDA owns the worker's replicas"
assert not [d for d in docs if d["kind"] == "HorizontalPodAutoscaler" and d["spec"]["scaleTargetRef"]["name"] == "ferrum-worker"]
PY2
tpl "${kedaon[@]}" --set-json 'keda.triggers=[{"type":"cron","metadata":{"timezone":"UTC","start":"0 8 * * *","end":"0 18 * * *","desiredReplicas":"2"}}]' | grep -q 'type: cron'
refuses "must never both scale one Deployment" -f "$chart/presets/split.yaml" --set keda.prometheusAddress=x --set-json 'pools[1]={"name":"worker","background":"only","replicas":1,"keda":{"enabled":true,"maxReplicas":2},"autoscaling":{"maxReplicas":2}}'
refuses "for a pool with background: only" -f "$chart/presets/split.yaml" --set keda.prometheusAddress=x --set-json 'pools[0]={"name":"web","background":"off","autoscaling":{"enabled":false},"keda":{"enabled":true}}'
refuses "set keda.prometheusAddress" -f "$chart/presets/split.yaml" --set-json 'pools[1]={"name":"worker","background":"only","autoscaling":{"enabled":false},"keda":{"enabled":true,"maxReplicas":2}}'
refuses "keda.minReplicas must be at least 1" "${kedaon[@]}" --set keda.minReplicas=0
refuses "worker (9 + 1 in a rollout) x (20 + 6)" "${kedaon[@]}" --set keda.maxReplicas=9 --set-json 'pools[1]={"name":"worker","background":"only","autoscaling":{"enabled":false},"keda":{"enabled":true},"dbMaxConnections":20}'

echo "each pool takes its own scheduling: node selector, tolerations, affinity, spread and priority"
tpl -f "$chart/presets/zones.yaml" --set-json 'pools[0].nodeSelector={"ferrum.dev/nodes":"private"}' \
  --set-json 'pools[0].tolerations=[{"key":"private","operator":"Exists","effect":"NoSchedule"}]' \
  --set-json 'pools[0].affinity={"podAntiAffinity":{"preferredDuringSchedulingIgnoredDuringExecution":[{"weight":1,"podAffinityTerm":{"topologyKey":"kubernetes.io/hostname","labelSelector":{"matchLabels":{"app.kubernetes.io/name":"ferrum"}}}}]}}' \
  --set-json 'pools[0].topologySpreadConstraints=[{"maxSkew":2,"topologyKey":"kubernetes.io/hostname","whenUnsatisfiable":"ScheduleAnyway","labelSelector":{"matchLabels":{"app.kubernetes.io/name":"ferrum"}}}]' \
  --set-json 'pools[1].priorityClassName="ferrum-public-low"' > "$tmp/sched.yaml"
python3 - "$tmp/sched.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
pod = {d["metadata"]["name"]: d["spec"]["template"]["spec"] for d in docs if d["kind"] == "Deployment"}
g, pub = pod["ferrum"], pod["ferrum-public"]
assert g["nodeSelector"] == {"ferrum.dev/nodes": "private"} and g["tolerations"][0]["key"] == "private"
assert "podAntiAffinity" in g["affinity"]
assert [c["maxSkew"] for c in g["topologySpreadConstraints"]] == [2], g["topologySpreadConstraints"]
assert pub["priorityClassName"] == "ferrum-public-low"
# What a pool does not set stays as before: no selector, no affinity, today's two spread rules.
assert "nodeSelector" not in pub and "affinity" not in pub and "priorityClassName" not in g
assert [c["topologyKey"] for c in pub["topologySpreadConstraints"]] == ["topology.kubernetes.io/zone", "kubernetes.io/hostname"]
PY2

echo "an app's calls are matched on X-Ferrum-UI, anchored, and other paths stay with the host"
python3 - "$tmp/pools.yaml" <<'PY'
import re, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
r = next(d for d in docs if d["kind"] == "HTTPRoute" and d["metadata"]["name"] == "ferrum-finance-0")
ms = r["spec"]["rules"][0]["matches"]
assert {m["path"]["value"] for m in ms} == {"/api", "/"}, ms
h = ms[0]["headers"][0]
assert h["name"] == "x-ferrum-ui" and h["type"] == "RegularExpression", h
rx = re.compile(h["value"])
assert rx.fullmatch("erp@1.2.3") and rx.fullmatch("books@0.1.0")
assert not rx.search("xerp@1") and not rx.search("erp") and not rx.search("erp@1@2")
default = next(d for d in docs if d["kind"] == "HTTPRoute" and d["metadata"]["name"] == "ferrum")
paths = {m["path"]["value"] for rule in default["spec"]["rules"] for m in rule.get("matches", [])}
assert {"/ws", "/mcp", "/files"} <= paths, paths
PY
echo "every server pool reads the audit sink's settings (docs/operations.md, Sending the audit log elsewhere)"
python3 - "$tmp/zones.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
pools = [d for d in docs if d["kind"] == "Deployment" and any(c["name"] == "ferrum" for c in d["spec"]["template"]["spec"]["containers"])]
assert len(pools) > 1, [p["metadata"]["name"] for p in pools]
for p in pools:
    env = {e["name"]: e for c in p["spec"]["template"]["spec"]["containers"] for e in c.get("env", [])}
    for k in ("FERRUM_AUDIT_SINK_URL", "FERRUM_AUDIT_SINK_SECRET"):
        assert env[k]["valueFrom"]["secretKeyRef"]["key"] == k, (p["metadata"]["name"], k)
PY
echo "zones that limit staff to some apps (level 2) reach every server pool as written, lists included"
tpl -f "$chart/presets/zones.yaml" \
  --set-json 'zones={"public":{"staff":"none"},"office":{"staff":"all"},"sales":{"staff":["crm","helpdesk"],"admin":false,"custom":true}}' \
  --set-json 'pools[0].zone="sales"' > "$tmp/zones-limited.yaml"
python3 - "$tmp/zones-limited.yaml" <<'PY'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
pools = [d for d in docs if d["kind"] == "Deployment" and any(c["name"] == "ferrum" for c in d["spec"]["template"]["spec"]["containers"])]
for p in pools:
    env = {e["name"]: e.get("value") for c in p["spec"]["template"]["spec"]["containers"] for e in c.get("env", [])}
    zones = json.loads(env["FERRUM_ZONES"])
    assert zones["sales"] == {"staff": ["crm", "helpdesk"], "admin": False, "custom": True}, zones
general = next(p for p in pools if p["metadata"]["name"] == "ferrum")
env = {e["name"]: e.get("value") for e in general["spec"]["template"]["spec"]["containers"][0].get("env", [])}
assert env["FERRUM_ZONE"] == "sales", env.get("FERRUM_ZONE")
PY
echo "a zone that asks staff to step up reaches every server pool as written (docs/operations.md, Asking again)"
tpl -f "$chart/presets/zones.yaml" \
  --set-json 'zones={"public":{"staff":"none"},"office":{"staff":"all"},"away":{"step_up":true,"networks":["203.0.113.0/24"]}}' > "$tmp/zones-step-up.yaml"
python3 - "$tmp/zones-step-up.yaml" <<'PY'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
pools = [d for d in docs if d["kind"] == "Deployment" and any(c["name"] == "ferrum" for c in d["spec"]["template"]["spec"]["containers"])]
assert pools
for p in pools:
    env = {e["name"]: e.get("value") for c in p["spec"]["template"]["spec"]["containers"] for e in c.get("env", [])}
    zones = json.loads(env["FERRUM_ZONES"])
    assert zones["away"] == {"step_up": True, "networks": ["203.0.113.0/24"]}, zones
PY
echo "ok"
