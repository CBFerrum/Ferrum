#!/usr/bin/env bash
# Deploys the Compose stack (database, Ferrum, PDF renderer, web app) and checks that it works as a
# deployment. A workspace is created, seeded and invoiced by the release being replaced
# (FERRUM_PREVIOUS_IMAGE, when there is one), then the server's container is replaced with
# FERRUM_IMAGE as an upgrade does: the session, the data and the web app's route to the new
# container must survive it. Then, on the new image: the security and caching headers; the web app
# serves its pages and scripts and passes the API and live updates through; a Sales Invoice is
# created, submitted and printed as HTML and PDF; the server recovers from a database restart; the
# database and the attachments are backed up, the stack is deleted with its volumes and restored
# from the backups, data, sessions and numbering intact; a short load test runs without errors.
# Everything is removed afterwards. CI runs it on every image it builds.
#
#   FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:latest FERRUM_WEB_IMAGE=ghcr.io/cbferrum/ferrum-frontend:latest \
#     deploy/smoke.sh     # needs Docker, curl, openssl, python3 + httpx
set -euo pipefail
cd "$(dirname "$0")"

NEW_IMAGE=${FERRUM_IMAGE:-ghcr.io/cbferrum/ferrum:latest}
export FERRUM_WEB_IMAGE=${FERRUM_WEB_IMAGE:-ghcr.io/cbferrum/ferrum-frontend:latest}
# One user drives the load test: lift the per-user budget for it.
export FERRUM_USER_REQUESTS_PER_MINUTE=${FERRUM_USER_REQUESTS_PER_MINUTE:-0}
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-ferrum-smoke}
# The web app's port, as the stack publishes it.
export FERRUM_PORT=${SMOKE_WEB_PORT:-8080}
export POSTGRES_PASSWORD FERRUM_DB_PASSWORD JWT_SECRET FERRUM_SECRET_KEY
POSTGRES_PASSWORD=$(openssl rand -hex 16)
FERRUM_DB_PASSWORD=$(openssl rand -hex 16)
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
PORT=${SMOKE_PORT:-5150}
API="http://127.0.0.1:${PORT}"
WEB="http://127.0.0.1:${FERRUM_PORT}"
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
    echo "--- smoke test failed; logs:"
    compose logs --tail 200 ferrum web pdf db || true
  fi
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
wait_up() {
  for _ in $(seq 1 90); do
    curl -fsS "$1" >/dev/null 2>&1 && return 0
    sleep 2
  done
  fail "$2 did not come up"
}
login() { curl -fsS "$1/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"smoke\",\"email\":\"smoke@smoke.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
invoice() {
  curl -fsS "${auth[@]}" -H 'content-type: application/json' -H "idempotency-key: smoke-$1-$(date +%s)" \
    "$API/api/v1/SalesInvoice?submit=true" -d "{\"company\":\"$company\",\"customer\":\"$customer\",\"items\":[{\"item\":\"$item\",\"qty\":\"2\"}]}"
}

export FERRUM_IMAGE=$NEW_IMAGE
if [ -n "${FERRUM_PREVIOUS_IMAGE:-}" ]; then
  if docker pull -q "$FERRUM_PREVIOUS_IMAGE" >/dev/null 2>&1 || docker image inspect "$FERRUM_PREVIOUS_IMAGE" >/dev/null 2>&1; then
    FERRUM_IMAGE=$FERRUM_PREVIOUS_IMAGE
  else
    echo "    no $FERRUM_PREVIOUS_IMAGE to upgrade from: the workspace starts on $NEW_IMAGE"
  fi
fi

step "starting the stack from $FERRUM_IMAGE and $FERRUM_WEB_IMAGE"
compose up -d --no-build db ferrum pdf web
wait_up "$API/_health" "the server"
wait_up "$WEB/healthz" "the web app"

step "creating, seeding and invoicing a workspace"
compose exec -T ferrum ferrum task create_tenant slug:smoke name:"Smoke Co" email:smoke@smoke.test password:"$PASSWORD" \
  packages:core,accounting,inventory,selling,buying,manufacturing,india
compose exec -T ferrum ferrum task seed_demo tenant:smoke
TOKEN=$(login "$API") || fail "signing in"
auth=(-H "authorization: Bearer $TOKEN")
company=$(curl -fsS "${auth[@]}" "$API/api/v1/Company?limit=1&fields=id" | json 'd["rows"][0]["id"]')
customer=$(curl -fsS "${auth[@]}" "$API/api/v1/Customer?limit=1&fields=id" | json 'd["rows"][0]["id"]')
item=$(curl -fsS "${auth[@]}" "$API/api/v1/Item?limit=1&fields=id&filters=%7B%22disabled%22%3Afalse%7D" | json 'd["rows"][0]["id"]')
before=$(invoice before | json 'd["id"]') || fail "invoicing on $FERRUM_IMAGE"

step "replacing the server's container with $NEW_IMAGE, as an upgrade does"
export FERRUM_IMAGE=$NEW_IMAGE
compose up -d --no-build --no-deps --force-recreate ferrum
wait_up "$API/_health" "the server"
[ "$(curl -fsS "${auth[@]}" "$API/api/auth/me" | json 'd["user"]["email"]')" = "smoke@smoke.test" ] || fail "the session after the replacement"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$before" | json 'd["docstatus"]')" = "1" ] || fail "the invoice made before the replacement"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$before/audit" | json 'd["chain_valid"]')" = "True" ] || fail "the audit chain across the replacement"
[ "$(curl -fsS --max-time 30 "${auth[@]}" "$WEB/api/v1/SalesInvoice/$before" | json 'd["id"]')" = "$before" ] || fail "the web app does not reach the new server container"

step "checking headers"
headers=$(curl -fsS -D - -o /dev/null "${auth[@]}" "$API/api/v1/Customer?limit=1")
for h in "cache-control: no-store" "x-content-type-options: nosniff" "strict-transport-security" "content-security-policy"; do
  grep -qi "^$h" <<<"$headers" || fail "missing header $h"
done

step "the web app and the API through it"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$WEB/")" =~ ^302\ .*/app/$ ]] || fail "/ does not lead to the app"
# A deep link: the app answers for its own routes.
page=$(curl -fsS -D "$work/page.h" "$WEB/app/SalesInvoice")
grep -q '<div id="root">' <<<"$page" || fail "the app's page"
for h in "content-security-policy: .*script-src 'self'" "x-content-type-options: nosniff" "cache-control: no-cache"; do
  grep -qi "^$h" "$work/page.h" || fail "the app's page lacks $h"
done
script=$(grep -o '/app/assets/[^"]*\.js' <<<"$page" | head -1)
curl -fsS -D "$work/script.h" -o /dev/null "$WEB$script" || fail "the app's script $script"
grep -qi "^cache-control: public, max-age=31536000, immutable" "$work/script.h" || fail "the app's script is not kept"
[ -n "$(login "$WEB")" ] || fail "signing in through the web app"
[ "$(curl -fsS "${auth[@]}" "$WEB/api/auth/me" | json 'd["user"]["email"]')" = "smoke@smoke.test" ] || fail "the API through the web app"
# Live updates: the WebSocket handshake gets through (101); curl then gives up on the open socket.
ws=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "${auth[@]}" -H 'connection: Upgrade' -H 'upgrade: websocket' \
  -H 'sec-websocket-version: 13' -H "sec-websocket-key: $(openssl rand -base64 16)" "$WEB/ws/live" 2>/dev/null || true)
[ "$ws" = 101 ] || fail "live updates through the web app (HTTP $ws)"

step "invoicing"
created=$(invoice after) || fail "invoicing"
id=$(json 'd["id"]' <<<"$created")
[ "$(json 'd["docstatus"]' <<<"$created")" = "1" ] || fail "the invoice was not submitted: $created"
echo "    $(json 'd["number"]' <<<"$created"), total $(json 'd["grand_total"]' <<<"$created")"
printed=$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print")
grep -q "Tax Invoice" <<<"$printed" || fail "the print"
# The renderer may still be starting.
for _ in $(seq 1 30); do
  curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/print?format=pdf" -o "$work/invoice.pdf" 2>/dev/null && break
  sleep 2
done
[ "$(head -c 5 "$work/invoice.pdf" 2>/dev/null)" = "%PDF-" ] || fail "the PDF"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/audit" | json 'd["chain_valid"]')" = "True" ] || fail "the audit chain"

step "restarting the database under the running server"
compose restart db
for _ in $(seq 1 30); do
  curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id" >/dev/null 2>&1 && break
  sleep 2
done
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id" | json 'd["id"]')" = "$id" ] || fail "the server did not recover from the database restart"

# The Compose stack's backup and restore. The dump is taken as the superuser: the application role
# is subject to the forced row-level security, so its pg_dump stops partway.
step "backing up, losing the stack with its volumes, and restoring"
attachment=$(curl -fsS "${auth[@]}" -F entity=SalesInvoice -F record_id="$id" -F "file=@$work/invoice.pdf;type=application/pdf" "$API/api/files") ||
  fail "attaching a file"
file_id=$(json 'd["id"]' <<<"$attachment")
file_sha=$(json 'd["sha256"]' <<<"$attachment")
compose exec -T db pg_dump -U postgres -Fc ferrum > "$work/ferrum.dump" || fail "the database backup"
compose exec -T ferrum tar -C /var/lib/ferrum/files -czf - . > "$work/files.tgz" || fail "the attachments backup"
compose down -v --remove-orphans
compose up -d db
# A new volume is initialized over the socket alone; the server is up once it listens on TCP.
for _ in $(seq 1 60); do
  compose exec -T db pg_isready -q -h 127.0.0.1 -U postgres -d ferrum && break
  sleep 2
done
compose exec -T db pg_restore -U postgres -d ferrum --clean --if-exists --exit-on-error < "$work/ferrum.dump" || fail "the database restore"
compose run --rm --no-deps -T --entrypoint tar ferrum -C /var/lib/ferrum/files -xzf - < "$work/files.tgz" || fail "the attachments restore"
compose up -d --no-build ferrum pdf web
wait_up "$API/_health" "the restored server"
wait_up "$WEB/healthz" "the web app"
[ "$(curl -fsS "${auth[@]}" "$API/api/auth/me" | json 'd["user"]["email"]')" = "smoke@smoke.test" ] || fail "the session after the restore"
[ "$(curl -fsS "${auth[@]}" "$WEB/api/v1/SalesInvoice/$id" | json 'd["docstatus"]')" = "1" ] || fail "the invoice after the restore"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id/audit" | json 'd["chain_valid"]')" = "True" ] || fail "the audit chain after the restore"
curl -fsSL "${auth[@]}" "$API/files/$file_id" -o "$work/restored.pdf" || fail "the attachment after the restore"
[ "$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$work/restored.pdf")" = "$file_sha" ] ||
  fail "the attachment's content after the restore"
# Numbering carries on from the restored series.
restored=$(invoice restored) || fail "invoicing after the restore"
[ "$(json 'd["docstatus"]' <<<"$restored")" = "1" ] || fail "the invoice after the restore was not submitted: $restored"
echo "    $(json 'd["number"]' <<<"$restored") after the restore"

# On the restored stack.
step "load test"
python3 ../tools/loadtest.py --url "$API" --tenant smoke --email smoke@smoke.test --password "$PASSWORD" --levels 10,50 --seconds "${SMOKE_LOAD_SECONDS:-20}"

step "the deployment works"
