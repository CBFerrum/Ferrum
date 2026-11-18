#!/usr/bin/env bash
# What the Kubernetes reference stack's database does that the product does not: the checks that were once part of the
# product's cluster smoke (deploy/kind/smoke.sh) and test CloudNativePG, not Ferrum, run against an installation made by
# reference/kubernetes/install.sh:
#   * the example database comes up as three instances;
#   * the primary is killed: writes resume on the new primary, nothing is lost;
#   * with a standby fenced off, growing or shrinking the database is refused, saying why; back, it goes to two instances
#     and three again under writes, and what is never safe is refused at once;
#   * a backup is taken (with a write after it, in the archived WAL) and restored into a new database cluster holding the
#     latest writes.
# It makes a workspace of its own and reaches the server through its Service (a port-forward), not through the gateway.
# Run on a cluster with the stack installed: reference/kubernetes/smoke.sh. Not run on any cluster yet.
set -euo pipefail
cd "$(dirname "$0")"

NS=${NS:-ferrum}
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
    k get cluster,backup -o wide || true
    k get events --sort-by=.lastTimestamp 2>/dev/null | tail -60 || true
    kubectl -n cnpg-system logs deploy/cnpg-controller-manager --tail=60 || true
    kubectl -n cnpg-system logs deploy/barman-cloud --tail=40 || true
  fi
  jobs -p | xargs -r kill 2>/dev/null || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT

step "the database: three instances"
k wait cluster/ferrum-db --for=condition=Ready --timeout=15m
[ "$(k get cluster ferrum-db -o jsonpath='{.status.readyInstances}')" = 3 ] || fail "not three database instances"
k rollout status deploy/ferrum --timeout=10m

step "a workspace and an invoice, through the server's Service"
k exec deploy/ferrum -- ferrum task create_tenant slug:reference name:"Reference Co" email:ops@reference.test password:"$PASSWORD" \
  packages:core,accounting,inventory,selling,buying,manufacturing,india
k exec deploy/ferrum -- ferrum task seed_demo tenant:reference
k port-forward svc/ferrum 18080:80 >"$work/forward.log" 2>&1 &
API=http://127.0.0.1:18080
for _ in $(seq 1 40); do curl -fs "$API/_ping" >/dev/null 2>&1 && break; sleep 0.5; done
c() { curl -sS --max-time 60 "$@"; }
TOKEN=$(c -f "$API/api/auth/login" -H 'content-type: application/json' \
  -d "{\"tenant\":\"reference\",\"email\":\"ops@reference.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]')
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
id=$(invoice "?submit=true" | json 'd["id"]')
upload() {
  head -c 300000 /dev/urandom >"$work/$1"
  c -f "${auth[@]}" -F "file=@$work/$1;type=application/octet-stream" -F entity=SalesInvoice -F record_id="$id" "$API/api/files" | json 'd["id"]'
}
file=$(upload first.bin)

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

step "growing or shrinking the database waits for its standbys (reference/kubernetes/scale-database.sh)"
# A standby fenced off: replication is impaired, so a change is refused, saying why, and nothing changes.
standby=$(k get cluster ferrum-db -o json | json '[n for n in d["status"]["instanceNames"] if n != d["status"]["currentPrimary"]][0]')
k annotate cluster ferrum-db cnpg.io/fencedInstances="[\"$standby\"]" --overwrite >/dev/null
for _ in $(seq 1 120); do [ "$(k get cluster ferrum-db -o jsonpath='{.status.readyInstances}')" -lt 3 ] && break; sleep 1; done
if out=$(./scale-database.sh --instances 2 2>&1); then fail "the database was changed while a standby was fenced: $out"; fi
grep -q "not changed: replication is not healthy" <<<"$out" || fail "the refusal does not say why: $out"
[ "$(k get cluster ferrum-db -o jsonpath='{.spec.instances}')" = 3 ] || fail "a refused change changed the database"
# Back, it goes ahead: two instances (one synchronous standby), then three again, with writes throughout.
k annotate cluster ferrum-db cnpg.io/fencedInstances- >/dev/null
./scale-database.sh --instances 2 --wait 600 || fail "two instances once the standby was back"
invoice "" -o /dev/null || fail "a write with two instances"
./scale-database.sh --instances 3 --wait 600 || fail "three instances again"
invoice "" -o /dev/null || fail "a write with three instances again"
# What is never safe is refused at once.
if out=$(./scale-database.sh --instances 1 2>&1); then fail "writes were left without a synchronous standby: $out"; fi
grep -q "synchronous standby" <<<"$out" || fail "$out"
if out=$(./scale-database.sh --storage 1Gi 2>&1); then fail "a volume shrank: $out"; fi
grep -q "volumes only grow" <<<"$out" || fail "$out"
class=$(kubectl get storageclass -o jsonpath='{.items[?(@.metadata.annotations.storageclass\.kubernetes\.io/is-default-class=="true")].metadata.name}')
if [ "$(kubectl get storageclass "$class" -o jsonpath='{.allowVolumeExpansion}')" != true ]; then
  if out=$(./scale-database.sh --storage 200Gi 2>&1); then fail "a volume grew on a class that cannot grow one: $out"; fi
  grep -q "cannot grow a volume" <<<"$out" || fail "$out"
fi
k wait cluster/ferrum-db --for=condition=Ready --timeout=10m

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

step "the reference stack works"
