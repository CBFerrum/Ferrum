#!/usr/bin/env bash
# Deploys the product chart (deploy/helm/ferrum) to the current cluster, prepared by install.sh with the images ferrum:ci and
# ferrum-editors:ci loaded, on the Kubernetes reference stack (reference/kubernetes/install.sh: an example database and
# bucket the chart is given the addresses of), and checks it the way an operator would:
#   * the server, the PDF renderer and the browser editors' server as two or three replicas each, the gateway as two proxies
#     with certificates from cert-manager;
#   * through the gateway: HTTPS only, the server's pages and API, no metrics, the
#     editor server on its own host with its policies; nothing else reaches the API or the editor
#     server (network policy); metrics only with their token; the autoscaler sees the API's CPU use;
#   * a workspace is created and seeded; someone signs in, submits a Sales Invoice, prints it as
#     HTML and PDF and attaches a file, which every replica then serves from object storage;
#   * the API is restarted replica by replica under traffic without a failed request;
#   * with a base domain (shapes B and C): one person signs in once, at the accounts host, and
#     enters two workspaces on their subdomains and one workspace's custom domain, each with a
#     session of its own; signing out everywhere ends all of them.
# What tests the example database (a primary lost, growing it, a backup restored) is reference/kubernetes/smoke.sh.
# CI runs it for every image published from main (.github/workflows/cluster.yml).
set -euo pipefail
cd "$(dirname "$0")"

NS=ferrum
HOST=erp.kind.test
PASSWORD='Cluster-Test-Passw0rd'
work=$(mktemp -d)

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
k() { kubectl -n "$NS" "$@"; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { k exec "$1" -c postgres -- psql -U postgres -d ferrum -tAc "$2"; }
primary() { k get cluster "${1:-ferrum-db}" -o jsonpath='{.status.currentPrimary}'; }

finish() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- cluster state"
    kubectl get pods -A -o wide || true
    k get cluster,backup,gateway,httproute,certificate,networkpolicy -o wide || true
    k get events --sort-by=.lastTimestamp 2>/dev/null | tail -60 || true
    for p in $(k get pods -l app.kubernetes.io/name=ferrum -o name 2>/dev/null); do
      echo "--- $p"
      k logs "$p" --tail=60 || true
    done
    kubectl -n cnpg-system logs deploy/cnpg-controller-manager --tail=60 || true
    kubectl -n envoy-gateway-system logs deploy/envoy-gateway --tail=40 || true
  fi
  jobs -p | xargs -r kill 2>/dev/null || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT

step "deploying: the reference stack's database and store, and the product on them"
# The Secret the chart reads (install.sh adds the store's key and a metrics token to it, where they are not there).
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
k create secret generic ferrum-secrets --from-literal=JWT_SECRET="$(openssl rand -base64 48 | tr -d '\n')" \
  --from-literal=FERRUM_METRICS_TOKEN="$(openssl rand -hex 32)" --dry-run=client -o yaml | k apply -f - >/dev/null
../../reference/kubernetes/install.sh -f values.yaml -d database.yaml
# kind has no load balancers: the proxies get a cluster address (this script port-forwards to it).
k patch envoyproxy ferrum --type merge -p '{"spec":{"provider":{"kubernetes":{"envoyService":{"type":"ClusterIP","externalTrafficPolicy":null}}}}}' >/dev/null
step "waiting for the server, the PDF renderer and the editor server"
for d in ferrum ferrum-pdf ferrum-editors; do k rollout status "deploy/$d" --timeout=10m; done
kubectl -n ferrum-office rollout status deploy/ferrum-office --timeout=10m
step "waiting for the gateway and its certificate"
k wait gateway/ferrum --for=condition=Programmed --timeout=5m
k wait certificate/ferrum-tls certificate/ferrum-editors-tls certificate/ferrum-office-tls --for=condition=Ready --timeout=5m
proxies="gateway.envoyproxy.io/owning-gateway-name=ferrum,gateway.envoyproxy.io/owning-gateway-namespace=$NS"
kubectl -n envoy-gateway-system wait deploy -l "$proxies" --for=condition=Available --timeout=5m
[ "$(kubectl -n envoy-gateway-system get deploy -l "$proxies" -o jsonpath='{.items[0].status.readyReplicas}')" = 2 ] || fail "not two gateway proxies"

svc=$(kubectl -n envoy-gateway-system get svc -o name -l "$proxies" | head -1)
[ -n "$svc" ] || fail "no service for the gateway"
kubectl -n envoy-gateway-system port-forward "$svc" 8443:443 8080:80 >"$work/gateway.log" 2>&1 &
# The CI root that signed it (install.sh).
k get secret ferrum-tls -o jsonpath='{.data.ca\.crt}' | base64 -d >"$work/ca.crt"
API="https://$HOST:8443"
# Through the gateway (port-forwarded: the Host header names no port, as from outside), with its
# certificate checked against the issuer.
c() { curl -sS --max-time 60 --cacert "$work/ca.crt" --resolve "$HOST:8443:127.0.0.1" --resolve "$HOST:8080:127.0.0.1" -H "Host: $HOST" "$@"; }
for attempt in $(seq 1 30); do
  c -fo /dev/null "$API/api/ready" 2>/dev/null && break
  if [ "$attempt" -eq 30 ]; then
    c -v -o /dev/null "$API/api/ready" 2>&1 | tail -30 || true
    cat "$work/gateway.log"
    kubectl -n envoy-gateway-system logs -l "$proxies" -c envoy --tail=30 || true
    k get httproute -o yaml | grep -A12 "^  status:" || true
    fail "the API is not reachable through the gateway"
  fi
  sleep 2
done

step "the gateway"
redirect=$(c -o /dev/null -w '%{http_code} %{redirect_url}' "http://$HOST:8080/login")
[ "$redirect" = "301 https://$HOST/login" ] || fail "plain HTTP is not redirected to HTTPS: $redirect"
grep -qi "<html" <<<"$(c -f "$API/login")" || fail "the sign-in page"
[[ "$(c -o /dev/null -w '%{http_code} %{redirect_url}' "$API/app/e/Customer")" =~ ^30[23]\ .*/open/Customer$ ]] || fail "today's web app's old addresses"
c -fo /dev/null "$API/.well-known/oauth-authorization-server" || fail "the OAuth metadata"
# Notifications on devices and installing (apps-ui.md §12.5): the core worker, Ferrum as an app.
grep -q 'addEventListener("push"' <<<"$(c -f "$API/sw.js")" || fail "the core worker"
[ "$(c -f "$API/manifest.webmanifest" | json 'd["id"] + " " + d["display"]')" = "/ standalone" ] || fail "the workspace's manifest"
if grep -q "ferrum_up" <<<"$(c "$API/metrics")"; then fail "metrics are reachable from outside"; fi

step "the editor server"
EDITORS=editors.kind.test
# Like c(): the Host header names no port, as from outside.
e() { curl -sS --max-time 60 --cacert "$work/ca.crt" --resolve "$EDITORS:8443:127.0.0.1" --resolve "$EDITORS:8080:127.0.0.1" -H "Host: $EDITORS" "$@"; }
redirect=$(e -o /dev/null -w '%{http_code} %{redirect_url}' "http://$EDITORS:8080/relay.html")
[ "$redirect" = "301 https://$EDITORS/relay.html" ] || fail "plain HTTP to the editor server is not redirected: $redirect"
grep -qi "frame-ancestors https://$HOST" <<<"$(e -fsD - -o /dev/null "https://$EDITORS:8443/relay.html")" || fail "the relay may be framed by others"
page=$(e -fsD "$work/editor-headers" "https://$EDITORS:8443/gridcraft/index.html")
grep -qi "script-src 'self' 'wasm-unsafe-eval' 'sha256-" "$work/editor-headers" || fail "an editor's page allows other scripts"
wasm=$(grep -o '[A-Za-z0-9._-]*_bg\.wasm' <<<"$page" | head -1)
grep -qi "^content-type: application/wasm" <<<"$(e -fsI "https://$EDITORS:8443/gridcraft/$wasm")" || fail "the editor's WebAssembly"
[ "$(k get deploy ferrum -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="FERRUM_EDITORS_URL")].value}')" = "https://$EDITORS" ] ||
  fail "the server does not know the editor server"

step "the online editor (Collabora, its own namespace and host)"
OFFICE=office.kind.test
o() { curl -sS --max-time 60 --cacert "$work/ca.crt" --resolve "$OFFICE:8443:127.0.0.1" --resolve "$OFFICE:8080:127.0.0.1" -H "Host: $OFFICE" "$@"; }
[ "$(o -o /dev/null -w '%{http_code} %{redirect_url}' "http://$OFFICE:8080/browser/x")" = "301 https://$OFFICE/browser/x" ] || fail "plain HTTP to the editor is not redirected"
# What is not for the public is sent back to Ferrum: discovery, the administration console, conversion.
for path in /hosting/discovery /cool/adminws /cool/convert-to/docx /browser/dist/admin/admin.html; do
  [ "$(o -o /dev/null -w '%{http_code} %{redirect_url}' "https://$OFFICE:8443$path")" = "302 https://$HOST/" ] || fail "$path is reachable from outside"
done
[ "$(k get deploy ferrum -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="FERRUM_OFFICE_ORIGIN")].value}')" = "https://$OFFICE" ] || fail "the server does not know the editor"
# The server fetches the editor's discovery over the cluster network, and finds the types Drive opens.
disc=$(k exec deploy/ferrum -- ferrum task doctor format:json 2>/dev/null || true)
grep -q '"check": "office"' <<<"$disc" || fail "the server's own report says nothing about the editor: $disc"

step "the network policy, metrics and the autoscaler"
# A pod in a namespace outside the gateway and monitoring reaches the cluster's API server (a control: it answers to anything
# that can connect), but gets no answer from the server, the editor server or the online editor.
kubectl create namespace policy-probe --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n policy-probe run policy-probe --image=curlimages/curl:8.21.0 --restart=Never --command -- sleep 600 >/dev/null
kubectl -n policy-probe wait pod/policy-probe --for=condition=Ready --timeout=3m >/dev/null
probe() { kubectl -n policy-probe exec policy-probe -- curl -sk -o /dev/null -m 5 -w '%{http_code}' "$1" 2>/dev/null || true; }
for attempt in $(seq 1 15); do
  [[ "$(probe "https://kubernetes.default.svc.cluster.local/")" =~ ^[1-5][0-9][0-9]$ ]] && break
  [ "$attempt" -lt 15 ] || fail "the probe pod reaches nothing, so it proves nothing about the policy"
  sleep 2
done
for _ in 1 2 3; do
  [ "$(probe "http://ferrum.$NS.svc.cluster.local/api/ready")" != 200 ] || fail "a pod outside the gateway reached the API"
done
for _ in 1 2 3; do
  [ "$(probe "http://ferrum-editors.$NS.svc.cluster.local/healthz")" != 200 ] || fail "a pod outside the gateway reached the editor server"
done
for _ in 1 2 3; do
  [ "$(probe "http://ferrum-office.ferrum-office.svc.cluster.local:9980/hosting/discovery")" != 200 ] || fail "a pod outside the gateway reached the online editor"
done
kubectl delete namespace policy-probe --wait=false >/dev/null
first=$(k get pods -l app.kubernetes.io/name=ferrum -o name | head -1)
k port-forward "$first" 15150:5150 >/dev/null 2>&1 &
metrics_forward=$!
for _ in $(seq 1 40); do curl -fs http://127.0.0.1:15150/_ping >/dev/null 2>&1 && break; sleep 0.5; done
token=$(k get secret ferrum-secrets -o jsonpath='{.data.FERRUM_METRICS_TOKEN}' | base64 -d)
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:15150/metrics)" = 401 ] || fail "metrics without the token"
grep -q "^ferrum_up 1" <<<"$(curl -fsS -H "authorization: Bearer $token" http://127.0.0.1:15150/metrics)" || fail "metrics with the token"
kill "$metrics_forward"
for attempt in $(seq 1 60); do
  cpu=$(k get hpa ferrum -o jsonpath='{.status.currentMetrics[0].resource.current.averageUtilization}')
  [ -n "$cpu" ] && break
  [ "$attempt" -lt 60 ] || fail "the autoscaler has no CPU figures"
  sleep 5
done
echo "    the API uses $cpu % of its CPU request; the autoscaler keeps 3 to 8 replicas"

step "creating and seeding a workspace"
k exec deploy/ferrum -- ferrum task create_tenant slug:cluster name:"Cluster Co" email:ops@cluster.test password:"$PASSWORD" \
  packages:core,accounting,inventory,selling,buying,manufacturing,india
k exec deploy/ferrum -- ferrum task seed_demo tenant:cluster

step "signing in and invoicing"
TOKEN=$(c -f "$API/api/auth/login" -H 'content-type: application/json' \
  -d "{\"tenant\":\"cluster\",\"email\":\"ops@cluster.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]')
auth=(-H "authorization: Bearer $TOKEN")
company=$(c -f "${auth[@]}" "$API/api/v1/Company?limit=1&fields=id" | json 'd["rows"][0]["id"]')
customer=$(c -f "${auth[@]}" "$API/api/v1/Customer?limit=1&fields=id" | json 'd["rows"][0]["id"]')
item=$(c -f "${auth[@]}" "$API/api/v1/Item?limit=1&fields=id&filters=%7B%22disabled%22%3Afalse%7D" | json 'd["rows"][0]["id"]')
invoice_body="{\"company\":\"$company\",\"customer\":\"$customer\",\"items\":[{\"item\":\"$item\",\"qty\":\"2\"}]}"
invoice() {
  local query=$1
  shift
  c -f "${auth[@]}" -H 'content-type: application/json' "$@" "$API/api/v1/SalesInvoice$query" -d "$invoice_body"
}
submitted=$(invoice "?submit=true")
id=$(json 'd["id"]' <<<"$submitted")
[ "$(json 'd["docstatus"]' <<<"$submitted")" = 1 ] || fail "the invoice was not submitted: $submitted"
grep -q "Tax Invoice" <<<"$(c -f "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print")" || fail "the print"
for attempt in $(seq 1 30); do
  c -f "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print?format=pdf" -o "$work/invoice.pdf" 2>/dev/null && break
  [ "$attempt" -lt 30 ] || fail "the PDF"
  sleep 2
done
[ "$(head -c 5 "$work/invoice.pdf")" = "%PDF-" ] || fail "the PDF is not a PDF"
[ "$(c -f "${auth[@]}" "$API/api/v1/SalesInvoice/$id/audit" | json 'd["chain_valid"]')" = True ] || fail "the audit chain"

step "attachments: object storage, served by every replica"
upload() {
  head -c 300000 /dev/urandom >"$work/$1"
  c -f "${auth[@]}" -F "file=@$work/$1;type=application/octet-stream" -F entity=SalesInvoice -F record_id="$id" "$API/api/files" | json 'd["id"]'
}
file=$(upload first.bin)
want=$(sha256sum <"$work/first.bin" | cut -d' ' -f1)
port=15150
for p in $(k get pods -l app.kubernetes.io/name=ferrum -o name); do
  port=$((port + 1))
  k port-forward "$p" "$port:5150" >/dev/null 2>&1 &
  forward=$!
  for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$port/_ping" >/dev/null 2>&1 && break; sleep 0.5; done
  got=$(curl -fsS "${auth[@]}" "http://127.0.0.1:$port/files/$file" | sha256sum | cut -d' ' -f1)
  kill "$forward"
  [ "$got" = "$want" ] || fail "$p does not serve the attachment"
done

step "restarting the API under traffic"
# Four clients at about five requests a second each: busy, and within one user's budget even on a
# single replica (FERRUM_USER_REQUESTS_PER_MINUTE), so every refusal is a real failure.
traffic() {
  local total=0 failed=0 code
  while [ ! -f "$work/stop" ]; do
    code=$(c -o /dev/null -w '%{http_code}' "${auth[@]}" "$API/api/v1/SalesInvoice/$id" 2>/dev/null || true)
    total=$((total + 1))
    if [ "$code" != 200 ]; then
      failed=$((failed + 1))
      echo "${code:-none}" >>"$work/failures"
    fi
    sleep 0.2
  done
  echo "$total $failed" >"$work/traffic.$1"
}
clients=()
for n in 1 2 3 4; do
  traffic "$n" &
  clients+=($!)
done
sleep 3
k rollout restart deploy/ferrum
k rollout status deploy/ferrum --timeout=10m
sleep 5
touch "$work/stop"
wait "${clients[@]}"
read -r total failed < <(cat "$work"/traffic.* | awk '{t += $1; f += $2} END {print t, f}')
echo "    $total requests while the replicas were replaced, $failed failed"
[ "$failed" = 0 ] || fail "requests failed during the restart: $(sort "$work/failures" | uniq -c | tr '\n' ' ')"

step "one person, many workspaces: subdomains, the accounts host, a custom domain"
BASE=ferrum.example
CUSTOM=shop.example.org
# Workspaces and the accounts host at *.ferrum.example and one custom domain, each listener with a certificate cert-manager
# has the CI root sign (added to the chart's Gateway and route here: the chart's listeners are for the hosts its values name).
k patch gateway ferrum --type=json -p '[
  {"op": "add", "path": "/spec/listeners/-", "value": {"name": "https-workspaces", "protocol": "HTTPS", "port": 443, "hostname": "*.ferrum.example", "tls": {"mode": "Terminate", "certificateRefs": [{"name": "ferrum-workspaces-tls"}]}, "allowedRoutes": {"namespaces": {"from": "Same"}}}},
  {"op": "add", "path": "/spec/listeners/-", "value": {"name": "https-custom", "protocol": "HTTPS", "port": 443, "hostname": "shop.example.org", "tls": {"mode": "Terminate", "certificateRefs": [{"name": "ferrum-custom-tls"}]}, "allowedRoutes": {"namespaces": {"from": "Same"}}}}]' >/dev/null
k patch httproute ferrum --type=json -p '[
  {"op": "add", "path": "/spec/parentRefs/-", "value": {"name": "ferrum", "sectionName": "https-workspaces"}},
  {"op": "add", "path": "/spec/parentRefs/-", "value": {"name": "ferrum", "sectionName": "https-custom"}},
  {"op": "add", "path": "/spec/hostnames/-", "value": "*.ferrum.example"},
  {"op": "add", "path": "/spec/hostnames/-", "value": "shop.example.org"}]' >/dev/null
k wait certificate/ferrum-workspaces-tls certificate/ferrum-custom-tls --for=condition=Ready --timeout=5m
k set env deploy/ferrum FERRUM_BASE_DOMAIN="$BASE" FERRUM_REGION=ci
k rollout status deploy/ferrum --timeout=10m
k exec deploy/ferrum -- ferrum task create_tenant slug:branch name:"Branch Co" email:ops@branch.test password:"$PASSWORD" packages:core
BRANCH=$(c -f "$API/api/auth/login" -H 'content-type: application/json' \
  -d "{\"tenant\":\"branch\",\"email\":\"ops@branch.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]')
# Pat is a member of both, with the same address and a password of each workspace's own.
person() { c -f -H "authorization: Bearer $1" -H 'content-type: application/json' "$API/api/admin/users" -o /dev/null \
  -d "{\"email\":\"pat@people.test\",\"full_name\":\"Pat Lee\",\"password\":\"$2\"}"; }
person "$TOKEN" Pat-Cluster-Pass-1
person "$BRANCH" Pat-Branch-Pass-1
# A browser through the gateway: a cookie jar for every host, the Host header without a port (as
# from outside), redirects followed by hand. `go` leaves the last answer's status in $got, its
# address in $last and its body in $work/page (call it directly, not in $(…): a subshell's
# variables never reach the checks).
host_of() { local h=${1#https://}; echo "${h%%/*}"; }
w() {
  local url=$1 host
  shift
  host=$(host_of "$url")
  curl -sS --max-time 60 --cacert "$work/ca.crt" --resolve "$host:8443:127.0.0.1" -H "Host: $host" -b "$work/jar" -c "$work/jar" "$@" \
    "https://$host:8443${url#https://$host}"
}
post() { w "$1" -H 'content-type: application/json' -H 'sec-fetch-site: same-origin' -H "origin: https://$(host_of "$1")" -d "$2"; }
go() {
  local url=$1 out
  for _ in $(seq 1 10); do
    out=$(w "$url" -o "$work/page" -w '%{http_code} %{redirect_url}')
    got=${out%% *}
    if [[ ! "$got" =~ ^30[12378]$ ]]; then
      last=$url
      return
    fi
    url=${out#* }
    url=${url/:8443/}
  done
  fail "too many redirects from $1"
}
param() { python3 -c "import sys, urllib.parse as u; print(u.parse_qs(u.urlsplit(sys.argv[1]).query)[sys.argv[2]][0])" "$1" "$2"; }
go "https://nobody.$BASE/"
[ "$got" = 404 ] || fail "a host of no workspace answered ($got)"
# The first workspace: to the accounts host, signing in there (no mail in this cluster: the
# workspace's password makes the Ferrum account at once), back with a code for a session of its own.
go "https://cluster.$BASE/"
[ "$got" = 200 ] && [[ "$last" == "https://accounts.$BASE/login?next="* ]] || fail "no way to the accounts host ($got $last)"
authorize=$(param "$last" next)
grep -q '"cookie":true' <<<"$(post "https://accounts.$BASE/api/accounts/sign-in" '{"email":"pat@people.test","password":"Pat-Cluster-Pass-1"}')" ||
  fail "signing in at the accounts host"
go "https://accounts.$BASE$authorize"
[ "$got" = 200 ] && [ "$last" = "https://cluster.$BASE/" ] || fail "not back in the workspace ($got $last)"
[ "$(w "https://cluster.$BASE/api/auth/me" | json 'd["user"]["email"]')" = pat@people.test ] || fail "the workspace's own session"
# The second: added to the account once, with its own password.
go "https://branch.$BASE/"
[ "$got" = 200 ] && [[ "$last" == "https://accounts.$BASE/authorize?"* ]] || fail "no step at the accounts host ($got $last)"
grep -q '"need":"join"' "$work/page" || fail "the second workspace was not offered to be added"
redirect=$(post "https://accounts.$BASE/api/accounts/join" \
  "{\"host\":\"branch.$BASE\",\"state\":\"$(param "$last" state)\",\"password\":\"Pat-Branch-Pass-1\"}" | json 'd["redirect"]')
go "$redirect"
[ "$got" = 200 ] && [ "$last" = "https://branch.$BASE/" ] || fail "not in the second workspace ($got $last)"
[ "$(w "https://branch.$BASE/api/auth/me" | json 'd["user"]["email"]')" = pat@people.test ] || fail "the second workspace's session"
# The second workspace's custom domain: its proof answers through the gateway; the server's own
# check would need public DNS, so the test marks it verified, as that check would.
proof=$(c -f -H "authorization: Bearer $BRANCH" -H 'content-type: application/json' "$API/api/admin/domains" -d "{\"host\":\"$CUSTOM\"}" | json 'd["verify_token"]')
[ "$(w "https://$CUSTOM/.well-known/ferrum-domain")" = "$proof" ] || fail "the custom domain's proof"
sql "$(primary)" "update tenant_host set verified_at = now() where host = '$CUSTOM'" >/dev/null
for attempt in $(seq 1 15); do
  go "https://$CUSTOM/"
  [ "$got" = 200 ] && [ "$last" = "https://$CUSTOM/" ] && break
  [ "$attempt" -lt 15 ] || fail "not in the workspace on its custom domain ($got $last)"
  sleep 2
done
[ "$(w "https://$CUSTOM/api/auth/me" | json 'd["user"]["email"]')" = pat@people.test ] || fail "the custom domain's session"
[ "$(w "https://accounts.$BASE/api/accounts/me" | json 'len([x for x in d["workspaces"] if x["joined"]])')" = 2 ] || fail "the account chooser"
# Signing out everywhere ends every session of the account.
post "https://accounts.$BASE/api/accounts/sign-out-everywhere" '{}' -o /dev/null
for h in "cluster.$BASE" "branch.$BASE" "$CUSTOM"; do
  [ "$(w "https://$h/api/auth/me" -o /dev/null -w '%{http_code}')" = 401 ] || fail "$h kept its session after signing out everywhere"
done
echo "    one Ferrum account: two workspaces on subdomains and one on its own domain, each its own session"

step "the deployment works"
