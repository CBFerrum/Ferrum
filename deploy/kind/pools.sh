#!/usr/bin/env bash
# Rehearses moving between deployment shapes with the Helm chart (docs/operations.md, "Moving from one shape
# to another") on the cluster install.sh prepared, with the image ferrum:ci loaded, on the Kubernetes reference stack
# (reference/kubernetes/install.sh: the example database and bucket the chart is given the addresses of):
#   * installs the chart with its default values (one pool), then upgrades it to presets/zones.yaml, and
#     proves that the first pool's Deployment was upgraded in place (same UID), that every rollout finished,
#     and that the server's Service answered every request meanwhile (a client that must see no failure);
#   * if the network plugin enforces NetworkPolicy: a pod carrying the public gateway's labels reaches the
#     public pool and none of the private ones, and one carrying the private gateway's labels the reverse;
#     where policies are not enforced it says so and skips, never passing for it;
#   * if the server answers GET /_zone, the public gateway answers from a zone that admits no staff;
#   * upgrades back to the default values: one Deployment again, the same one, with no failed request.
# Written from deploy/kind/smoke.sh and .github/workflows/cluster.yml; it has not been run anywhere yet.
# CI runs it as a job that cannot fail the workflow (.github/workflows/cluster.yml, job `pools`).
set -euo pipefail
cd "$(dirname "$0")/../.."

NS=ferrum
CHART=deploy/helm/ferrum
HOST=erp.kind.test
PROBE_IMAGE=curlimages/curl:8.21.0
GATEWAYS=envoy-gateway-system
work=$(mktemp -d)

step() { echo "==> $*"; }
skip() { echo "    SKIPPED: $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
k() { kubectl -n "$NS" "$@"; }

finish() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- state"
    kubectl get pods -A -o wide || true
    k get deploy,svc,networkpolicy,gateway,httproute -o wide || true
    k get events --sort-by=.lastTimestamp 2>/dev/null | tail -40 || true
  fi
  jobs -p | xargs -r kill 2>/dev/null || true
  kubectl -n "$GATEWAYS" delete pod -l ferrum.dev/probe --wait=false >/dev/null 2>&1 || true
  kubectl delete namespace pools-probe --wait=false >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT

# The values kind needs, over the reference stack's (reference/kubernetes/ferrum-values.yaml): the loaded image, no online
# editor and no browser editors, and its own names for what the chart ships as placeholders (given last, so that a preset's do
# not come back).
cat >"$work/kind.yaml" <<'EOF'
office: {enabled: false}
editors: {enabled: false}
privateGateway: {clusterIssuer: letsencrypt, hostnames: [erp.internal.kind.test]}
EOF
upgrade() {
  # --wait would hold the first install: the server is not ready until the bucket exists (made by install.sh, once).
  helm upgrade --install ferrum "$CHART" -n "$NS" "$@" -f reference/kubernetes/ferrum-values.yaml -f deploy/kind/values.yaml \
    -f "$work/kind.yaml" --timeout 20m
}
servers() { k get deploy -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | grep -E '^ferrum($|-general$|-public$|-finance$|-worker$)' | sort | tr '\n' ' '; }
rollouts() { for d in $(servers); do k rollout status "deploy/$d" --timeout=10m; done; }

# A pod in the gateways' namespace with a gateway's owning labels (never Envoy's own name label, so no
# ReplicaSet of Envoy Gateway adopts it): what the pools' network policies admit or refuse.
probe_pod() { # name gateway
  kubectl -n "$GATEWAYS" delete pod "$1" --ignore-not-found --wait=true >/dev/null
  kubectl -n "$GATEWAYS" run "$1" --image="$PROBE_IMAGE" --restart=Never \
    --labels="ferrum.dev/probe=1,gateway.envoyproxy.io/owning-gateway-name=$2,gateway.envoyproxy.io/owning-gateway-namespace=$NS" \
    --command -- sleep 900 >/dev/null
  kubectl -n "$GATEWAYS" wait "pod/$1" --for=condition=Ready --timeout=3m >/dev/null
}
code_from() { kubectl -n "$GATEWAYS" exec "$1" -- curl -s -o /dev/null -m 5 -w '%{http_code}' "$2" 2>/dev/null || true; }

# A client that asks the first pool's Service for /api/ready five times a second, from a pod every shape's
# policy admits (the private gateway's labels, in the gateways' namespace), until told to stop.
traffic_start() {
  kubectl -n "$GATEWAYS" delete pod pools-traffic --ignore-not-found --wait=true >/dev/null
  kubectl -n "$GATEWAYS" run pools-traffic --image="$PROBE_IMAGE" --restart=Never \
    --labels="ferrum.dev/probe=1,gateway.envoyproxy.io/owning-gateway-name=$NS-private,gateway.envoyproxy.io/owning-gateway-namespace=$NS" \
    --command -- sh -c "while :; do curl -s -o /dev/null -m 5 -w '%{http_code}\n' http://ferrum.$NS.svc.cluster.local/api/ready; sleep 0.2; done" >/dev/null
  kubectl -n "$GATEWAYS" wait pod/pools-traffic --for=condition=Ready --timeout=3m >/dev/null
  sleep 3
}
traffic_stop() { # what: says how many requests failed, and fails when any did
  sleep 5
  kubectl -n "$GATEWAYS" logs pools-traffic >"$work/traffic.log"
  kubectl -n "$GATEWAYS" delete pod pools-traffic --wait=false >/dev/null
  local total failed
  total=$(grep -c . "$work/traffic.log" || true)
  failed=$(grep -vc '^200$' "$work/traffic.log" || true)
  echo "    $total requests while $1, $failed failed"
  [ "$total" -gt 20 ] || fail "too few requests to tell anything while $1 ($total)"
  [ "$failed" = 0 ] || fail "requests failed while $1: $(grep -v '^200$' "$work/traffic.log" | sort | uniq -c | tr '\n' ' ')"
}

step "the reference stack, and the chart with its default values"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
k create secret generic ferrum-secrets --from-literal=JWT_SECRET="$(openssl rand -base64 48 | tr -d '\n')" \
  --from-literal=FERRUM_METRICS_TOKEN="$(openssl rand -hex 32)" --dry-run=client -o yaml | k apply -f - >/dev/null
reference/kubernetes/install.sh -f deploy/kind/values.yaml -f "$work/kind.yaml" -d deploy/kind/database.yaml
rollouts
[ "$(servers)" = "ferrum " ] || fail "the default values should make one server Deployment, not: $(servers)"
uid=$(k get deploy ferrum -o jsonpath='{.metadata.uid}')

step "upgrading to the zones preset under traffic"
traffic_start
upgrade -f "$CHART/presets/zones.yaml"
rollouts
traffic_stop "moving to zones"
[ "$(servers)" = "ferrum ferrum-finance ferrum-public ferrum-worker " ] || fail "zones should make four pools, not: $(servers)"
[ "$(k get deploy ferrum -o jsonpath='{.metadata.uid}')" = "$uid" ] || fail "the first pool's Deployment was replaced, not upgraded in place"
k wait gateway/ferrum-private --for=condition=Programmed --timeout=5m

step "which gateway reaches which pool"
kubectl create namespace pools-probe --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n pools-probe run plain --image="$PROBE_IMAGE" --restart=Never --command -- sleep 600 >/dev/null
kubectl -n pools-probe wait pod/plain --for=condition=Ready --timeout=3m >/dev/null
plain() { kubectl -n pools-probe exec plain -- curl -sk -o /dev/null -m 5 -w '%{http_code}' "$1" 2>/dev/null || true; }
for attempt in $(seq 1 15); do
  [[ "$(plain "https://kubernetes.default.svc.cluster.local/")" =~ ^[1-5][0-9][0-9]$ ]] && break
  [ "$attempt" -lt 15 ] || fail "a plain pod reaches nothing, so it proves nothing about the policies"
  sleep 2
done
if [ "$(plain "http://ferrum.$NS.svc.cluster.local/api/ready")" = 200 ]; then
  skip "network policy is not enforced here (a pod with no gateway's labels reached the server): nothing about the gateways is proven"
else
  probe_pod pools-public "$NS"
  probe_pod pools-private "$NS-private"
  worker_ip=$(k get pod -l app.kubernetes.io/name=ferrum-worker -o jsonpath='{.items[0].status.podIP}')
  url() { echo "http://$1.$NS.svc.cluster.local/api/ready"; }
  [ "$(code_from pools-public "$(url ferrum-public)")" = 200 ] || fail "the public gateway's proxies do not reach the public pool"
  for p in ferrum ferrum-finance; do
    [ "$(code_from pools-public "$(url $p)")" != 200 ] || fail "the public gateway's proxies reach $p, a private pool"
    [ "$(code_from pools-private "$(url $p)")" = 200 ] || fail "the private gateway's proxies do not reach $p"
  done
  [ "$(code_from pools-private "$(url ferrum-public)")" != 200 ] || fail "the private gateway's proxies reach the public pool"
  for g in pools-public pools-private; do
    [ "$(code_from $g "http://$worker_ip:5150/_ping")" != 200 ] || fail "$g reaches the worker pool, which takes no requests"
  done
  echo "    the public gateway reaches only the public pool; the private gateway only the private ones"
fi

step "the zone the public gateway answers from"
proxies="gateway.envoyproxy.io/owning-gateway-name=ferrum,gateway.envoyproxy.io/owning-gateway-namespace=$NS"
k wait certificate/ferrum-tls --for=condition=Ready --timeout=5m >/dev/null
svc=$(kubectl -n "$GATEWAYS" get svc -o name -l "$proxies" | head -1)
[ -n "$svc" ] || fail "no service for the public gateway"
kubectl -n "$GATEWAYS" port-forward "$svc" 8443:443 >"$work/gateway.log" 2>&1 &
forward=$!
k get secret ferrum-tls -o jsonpath='{.data.ca\.crt}' | base64 -d >"$work/ca.crt"
c() { curl -sS --max-time 30 --cacert "$work/ca.crt" --resolve "$HOST:8443:127.0.0.1" "$@"; }
for attempt in $(seq 1 30); do
  c -fo /dev/null "https://$HOST:8443/api/ready" 2>/dev/null && break
  [ "$attempt" -lt 30 ] || fail "the public gateway does not answer"
  sleep 2
done
zone_code=$(c -o "$work/zone.json" -w '%{http_code}' "https://$HOST:8443/_zone" || true)
if [ "$zone_code" = 404 ]; then
  skip "this server has no GET /_zone yet"
else
  [ "$zone_code" = 200 ] || fail "GET /_zone answered $zone_code"
  python3 - "$work/zone.json" <<'PY' || fail "the public gateway answers from a zone that admits staff: $(cat "$work/zone.json")"
import json, sys
d = json.load(open(sys.argv[1]))
rule = d.get("rule") if isinstance(d.get("rule"), dict) else d
assert rule.get("staff") == "none", d
PY
  echo "    the public gateway answers from a zone that admits no staff"
fi
kill "$forward" 2>/dev/null || true

step "back to the default values under traffic"
traffic_start
upgrade
rollouts
traffic_stop "moving back to one pool"
[ "$(servers)" = "ferrum " ] || fail "the default values should leave one server Deployment, not: $(servers)"
[ "$(k get deploy ferrum -o jsonpath='{.metadata.uid}')" = "$uid" ] || fail "the first pool's Deployment was replaced on the way back"
if k get gateway ferrum-private >/dev/null 2>&1; then fail "the private gateway is still there"; fi

step "the moves were rehearsed"
