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
refuses "more than database.connectionLimit 370" --set-json 'pools=[{"name":"web"},{"name":"b"}]'
# 2 x 34 + 2 x 34 + 20 fits; the migrate Job's 34 more, with a limit of 180, does not.
tpl "${small[@]}" --set database.connectionLimit=180 --set-json 'pools=[{"name":"web"},{"name":"b"}]' > /dev/null
refuses "the migrate Job 30 + 4" "${small[@]}" --set database.connectionLimit=180 --set migrate.enabled=true --set-json 'pools=[{"name":"web"},{"name":"b"}]' --is-upgrade
refuses "cannot be background: only" --set-json 'pools=[{"name":"web","background":"only"}]'
refuses "has no routes" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","background":"only","routes":[{"apps":["x"]}]}]'
refuses "both take all of host h.example.com" "${small[@]}" --set-json 'pools=[{"name":"web"},{"name":"b","routes":[{"hostnames":["h.example.com"]}]},{"name":"c","routes":[{"hostnames":["h.example.com"]}]}]'

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
echo "ok"
