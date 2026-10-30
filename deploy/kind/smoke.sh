#!/usr/bin/env bash
# Deploys deploy/k8s (through deploy/kind/kustomization.yaml) to the current cluster, prepared by
# install.sh with the images ferrum:ci and ferrum-editors:ci loaded, and checks it the way an
# operator would:
#   * the database comes up as three instances, the server, the PDF renderer and the browser
#     editors' server as two or three replicas each, the gateway as two proxies with certificates
#     from cert-manager;
#   * through the gateway: HTTPS only, the server's pages and API, no metrics, the
#     editor server on its own host with its policies; nothing else reaches the API or the editor
#     server (network policy); metrics only with their token; the autoscaler sees the API's CPU use;
#   * a workspace is created and seeded; someone signs in, submits a Sales Invoice, prints it as
#     HTML and PDF and attaches a file, which every replica then serves from object storage;
#   * the database primary is killed: writes resume on the new primary, nothing is lost;
#   * the API is restarted replica by replica under traffic without a failed request;
#   * a backup is taken and restored into a new database cluster holding the latest writes.
# CI runs it for every image published from main (.github/workflows/cluster.yml).
set -euo pipefail
cd "$(dirname "$0")"

NS=ferrum
HOST=erp.example.com
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
    kubectl -n cnpg-system logs deploy/barman-cloud --tail=40 || true
    kubectl -n envoy-gateway-system logs deploy/envoy-gateway --tail=40 || true
  fi
  jobs -p | xargs -r kill 2>/dev/null || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT

step "deploying"
kubectl apply -f ../k8s/namespace.yaml
secret() { k create secret generic "$@" --dry-run=client -o yaml | k apply -f -; }
secret ferrum-secrets \
  --from-literal=JWT_SECRET="$(openssl rand -base64 48 | tr -d '\n')" \
  --from-literal=FERRUM_SECRET_KEY="$(openssl rand -base64 32 | tr -d '\n')" \
  --from-literal=FERRUM_BOOTSTRAP_TOKEN="" \
  --from-literal=FERRUM_METRICS_TOKEN="$(openssl rand -hex 32)" \
  --from-literal=AWS_ACCESS_KEY_ID=test --from-literal=AWS_SECRET_ACCESS_KEY=test
secret ferrum-backup-s3 --from-literal=ACCESS_KEY_ID=test --from-literal=ACCESS_SECRET_KEY=test
kubectl kustomize . > "$work/manifests.yaml"
for attempt in $(seq 1 20); do
  kubectl apply -f "$work/manifests.yaml" && break
  [ "$attempt" -lt 20 ] || fail "the manifests were not accepted"
  sleep 5
done

step "waiting for the database (three instances)"
k wait cluster/ferrum-db --for=condition=Ready --timeout=15m
[ "$(k get cluster ferrum-db -o jsonpath='{.status.readyInstances}')" = 3 ] || fail "not three database instances"
step "waiting for the server, the PDF renderer and the editor server"
for d in ferrum ferrum-pdf ferrum-editors; do k rollout status "deploy/$d" --timeout=10m; done
step "waiting for the gateway and its certificate"
k wait gateway/ferrum --for=condition=Programmed --timeout=5m
k wait certificate/ferrum-tls certificate/ferrum-editors-tls --for=condition=Ready --timeout=5m
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
if grep -q "ferrum_up" <<<"$(c "$API/metrics")"; then fail "metrics are reachable from outside"; fi

step "the editor server"
EDITORS=editors.example.com
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

step "the network policy, metrics and the autoscaler"
# A pod outside the gateway and monitoring reaches the S3 emulator (which no policy guards) but
# gets no answer from the API.
kubectl -n ci-s3 run policy-probe --image=curlimages/curl:8.21.0 --restart=Never --command -- sleep 600 >/dev/null
kubectl -n ci-s3 wait pod/policy-probe --for=condition=Ready --timeout=3m >/dev/null
probe() { kubectl -n ci-s3 exec policy-probe -- curl -s -o /dev/null -m 5 -w '%{http_code}' "$1" 2>/dev/null || true; }
for attempt in $(seq 1 15); do
  [[ "$(probe "http://s3.ci-s3.svc.cluster.local:5000/")" =~ ^[1-5][0-9][0-9]$ ]] && break
  [ "$attempt" -lt 15 ] || fail "the probe pod reaches nothing, so it proves nothing about the policy"
  sleep 2
done
for _ in 1 2 3; do
  [ "$(probe "http://ferrum.$NS.svc.cluster.local/api/ready")" != 200 ] || fail "a pod outside the gateway reached the API"
done
for _ in 1 2 3; do
  [ "$(probe "http://ferrum-editors.$NS.svc.cluster.local/healthz")" != 200 ] || fail "a pod outside the gateway reached the editor server"
done
kubectl -n ci-s3 delete pod policy-probe --wait=false >/dev/null
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
echo "    the API uses $cpu % of its CPU request; the autoscaler keeps 3 to 10 replicas"

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

step "losing the database primary"
old=$(primary)
k delete pod "$old" --wait=false
start=$(date +%s)
# The old primary stops accepting work while it shuts down; a standby that has every committed
# transaction (synchronous replication) is promoted, and the API reconnects to it.
until now=$(primary) && [ -n "$now" ] && [ "$now" != "$old" ]; do
  [ $(($(date +%s) - start)) -lt 300 ] || fail "no standby promoted 300 s after $old was lost"
  sleep 1
done
promoted=$(($(date +%s) - start))
until invoice "" --max-time 10 -o /dev/null 2>/dev/null; do
  [ $(($(date +%s) - start)) -lt 300 ] || fail "no writes 300 s after the primary was lost"
  sleep 1
done
outage=$(($(date +%s) - start))
echo "    $now promoted after $promoted s; writes resumed $outage s after $old was lost"
[ "$outage" -le 90 ] || fail "writes were held up for $outage s"
for _ in $(seq 1 20); do invoice "" -o /dev/null || fail "a write after the failover"; done
[ "$(c -f "${auth[@]}" "$API/api/v1/SalesInvoice/$id" | json 'd["docstatus"]')" = 1 ] || fail "the invoice after the failover"
k wait cluster/ferrum-db --for=condition=Ready --timeout=10m

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

step "backing up"
k apply -f - <<'EOF'
apiVersion: postgresql.cnpg.io/v1
kind: Backup
metadata:
  name: ferrum-db-ci
spec:
  cluster:
    name: ferrum-db
  method: plugin
  pluginConfiguration:
    name: barman-cloud.cloudnative-pg.io
EOF
for attempt in $(seq 1 120); do
  phase=$(k get backup ferrum-db-ci -o jsonpath='{.status.phase}')
  [ "$phase" = completed ] && break
  [ "$phase" != failed ] || fail "the backup failed: $(k get backup ferrum-db-ci -o jsonpath='{.status.error}')"
  [ "$attempt" -lt 120 ] || fail "the backup did not complete"
  sleep 5
done
# A write after the base backup, to be recovered from the archived WAL.
later=$(upload later.bin)
segment=$(sql "$(primary)" "select pg_walfile_name(pg_switch_wal())")
for attempt in $(seq 1 60); do
  archived=$(sql "$(primary)" "select coalesce(last_archived_wal, '') from pg_stat_archiver")
  [[ "$archived" < "$segment" ]] || break
  [ "$attempt" -lt 60 ] || fail "WAL $segment was not archived (last: $archived)"
  sleep 2
done

step "restoring into a new database cluster"
k apply -f restore.yaml
k wait cluster/ferrum-db-restored --for=condition=Ready --timeout=15m
found=$(sql "$(primary ferrum-db-restored)" "select count(*) from file_object where id in ('$file', '$later')")
[ "$found" = 2 ] || fail "the restored database lacks writes ($found of 2 attachments)"
k delete cluster ferrum-db-restored --wait=false

step "the deployment works"
