#!/usr/bin/env bash
# Deploys the Compose stack (database, Ferrum, PDF renderer) from an image and checks that it works
# as a deployment: a workspace is created and seeded; someone signs in, lists, creates and submits a
# Sales Invoice and prints it as HTML and PDF; the security and caching headers are there; the
# server comes back from a restart with sessions intact; a short load test runs without errors.
# Everything is removed afterwards. CI runs it on every image it builds.
#
#   FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:latest deploy/smoke.sh     # needs Docker, curl, python3 + httpx
set -euo pipefail
cd "$(dirname "$0")"

export FERRUM_IMAGE=${FERRUM_IMAGE:-ghcr.io/cbferrum/ferrum:latest}
# One user drives the load test: lift the per-user budget for it.
export FERRUM_USER_REQUESTS_PER_MINUTE=${FERRUM_USER_REQUESTS_PER_MINUTE:-0}
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-ferrum-smoke}
export POSTGRES_PASSWORD FERRUM_DB_PASSWORD JWT_SECRET FERRUM_SECRET_KEY
POSTGRES_PASSWORD=$(openssl rand -hex 16)
FERRUM_DB_PASSWORD=$(openssl rand -hex 16)
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
PORT=${SMOKE_PORT:-5150}
API="http://127.0.0.1:${PORT}"
PASSWORD='Smoke-Test-Passw0rd'

work=$(mktemp -d)
cat > "$work/publish.yml" <<EOF
services:
  ferrum:
    ports: ["127.0.0.1:${PORT}:5150"]
EOF
compose() { docker compose -f docker-compose.yml -f "$work/publish.yml" "$@"; }
cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- smoke test failed; server log:"
    compose logs --tail 300 ferrum || true
  fi
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
wait_healthy() {
  for _ in $(seq 1 90); do
    curl -fsS "$API/_health" >/dev/null 2>&1 && return 0
    sleep 2
  done
  fail "the server did not become healthy"
}

step "starting the stack from $FERRUM_IMAGE"
compose up -d --no-build db ferrum pdf
wait_healthy

step "creating and seeding a workspace"
compose exec -T ferrum ferrum task create_tenant slug:smoke name:"Smoke Co" email:smoke@smoke.test password:"$PASSWORD" \
  packages:core,accounting,inventory,selling,buying,manufacturing,india
compose exec -T ferrum ferrum task seed_demo tenant:smoke

step "signing in"
login() { curl -fsS "$API/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"smoke\",\"email\":\"smoke@smoke.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
TOKEN=$(login)
auth=(-H "authorization: Bearer $TOKEN")
[ "$(curl -fsS "${auth[@]}" "$API/api/auth/me" | json 'd["user"]["email"]')" = "smoke@smoke.test" ] || fail "who am I"

step "checking headers"
headers=$(curl -fsS -D - -o /dev/null "${auth[@]}" "$API/api/v1/Customer?limit=1")
for h in "cache-control: no-store" "x-content-type-options: nosniff" "strict-transport-security" "content-security-policy"; do
  grep -qi "^$h" <<<"$headers" || fail "missing header $h"
done

step "invoicing"
company=$(curl -fsS "${auth[@]}" "$API/api/v1/Company?limit=1&fields=id" | json 'd["rows"][0]["id"]')
customer=$(curl -fsS "${auth[@]}" "$API/api/v1/Customer?limit=1&fields=id" | json 'd["rows"][0]["id"]')
item=$(curl -fsS "${auth[@]}" "$API/api/v1/Item?limit=1&fields=id&filters=%7B%22disabled%22%3Afalse%7D" | json 'd["rows"][0]["id"]')
invoice=$(curl -fsS "${auth[@]}" -H 'content-type: application/json' -H "idempotency-key: smoke-$(date +%s)" \
  "$API/api/v1/SalesInvoice?submit=true" -d "{\"company\":\"$company\",\"customer\":\"$customer\",\"items\":[{\"item\":\"$item\",\"qty\":\"2\"}]}")
id=$(json 'd["id"]' <<<"$invoice")
[ "$(json 'd["docstatus"]' <<<"$invoice")" = "1" ] || fail "the invoice was not submitted: $invoice"
echo "    $(json 'd["number"]' <<<"$invoice"), total $(json 'd["grand_total"]' <<<"$invoice")"
printed=$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print")
grep -q "Tax Invoice" <<<"$printed" || fail "the print"
# The renderer may still be starting.
for _ in $(seq 1 30); do
  curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print?format=pdf" -o "$work/invoice.pdf" 2>/dev/null && break
  sleep 2
done
[ "$(head -c 5 "$work/invoice.pdf" 2>/dev/null)" = "%PDF-" ] || fail "the PDF"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/audit" | json 'd["chain_valid"]')" = "True" ] || fail "the audit chain"

step "restarting the server"
compose restart ferrum >/dev/null
wait_healthy
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id" | json 'd["id"]')" = "$id" ] || fail "the session or the data after a restart"

step "load test"
python3 ../tools/loadtest.py --url "$API" --tenant smoke --email smoke@smoke.test --password "$PASSWORD" --levels 10,50 --seconds "${SMOKE_LOAD_SECONDS:-20}"

step "the deployment works"
