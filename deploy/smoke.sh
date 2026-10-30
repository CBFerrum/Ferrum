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
# Everything is removed afterwards. CI runs it on every image it builds. With FERRUM_EDITORS_IMAGE,
# the browser editors' server (editors.yml) runs beside the stack and is checked too; with
# SMOKE_MEET=1, the meeting server (meet.yml), which must accept the passes Ferrum signs, and
# calendar apps signing in with an app password.
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
files=(-f docker-compose.yml -f "$work/publish.yml")
if [ -n "${FERRUM_EDITORS_IMAGE:-}" ]; then
  files+=(-f editors.yml)
  export FERRUM_EDITORS_PORT=${SMOKE_EDITORS_PORT:-8090}
  export FERRUM_EDITORS_URL="http://127.0.0.1:$FERRUM_EDITORS_PORT"
fi
if [ -n "${SMOKE_MEET:-}" ]; then
  files+=(-f meet.yml)
  export FERRUM_MEET_PORT=${SMOKE_MEET_PORT:-7880} FERRUM_MEET_KEY=smoke FERRUM_MEET_SECRET FERRUM_MEET_EXTERNAL_IP=false FERRUM_MEET_NODE_IP=127.0.0.1
  export FERRUM_MEET_URL="ws://127.0.0.1:$FERRUM_MEET_PORT"
  FERRUM_MEET_SECRET=$(openssl rand -hex 32)
fi
compose() { docker compose "${files[@]}" "$@"; }
cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- smoke test failed; logs:"
    compose logs --tail 200 ferrum web pdf db ${FERRUM_EDITORS_IMAGE:+editors} ${SMOKE_MEET:+meet} || true
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
compose up -d --no-build db ferrum pdf web ${FERRUM_EDITORS_IMAGE:+editors} ${SMOKE_MEET:+meet}
wait_up "$API/_health" "the server"
wait_up "$WEB/healthz" "the web app"

step "creating, seeding and invoicing a workspace"
compose exec -T ferrum ferrum task create_tenant slug:smoke name:"Smoke Co" email:smoke@smoke.test password:"$PASSWORD" \
  packages:core,accounting,inventory,selling,buying,manufacturing,india${SMOKE_MEET:+,comms}
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

step "the shell: browser sessions, the platform's files, app pages (docs/plan/apps-ui.md)"
curl -fsS -D "$work/runtime.h" -o /dev/null "$API/_shell/v1/runtime.js" || fail "the shell's runtime"
grep -qi "^content-type: text/javascript" "$work/runtime.h" || fail "the shell's runtime is not JavaScript"
[ "$(curl -sS -o /dev/null -w '%{http_code}' "$API/login")" = 200 ] || fail "the sign-in page"
creds="\"tenant\":\"smoke\",\"email\":\"smoke@smoke.test\",\"password\":\"$PASSWORD\""
# An API client gets a token and no cookie; a page of the site gets the cookie.
curl -fsS -D "$work/api-login.h" -o /dev/null -H 'content-type: application/json' -d "{$creds}" "$API/api/auth/login" || fail "signing in as an API client"
! grep -qi "^set-cookie" "$work/api-login.h" || fail "an API client got a session cookie"
page_login="{$creds,\"session\":\"cookie\"}"
[ "$(curl -fsS -c "$work/jar" -D "$work/page-login.h" -H 'content-type: application/json' -H 'sec-fetch-site: same-origin' -d "$page_login" "$API/api/auth/login" | json 'd["cookie"]')" = True ] \
  || fail "signing in from a page"
grep -qi "^set-cookie: .*; Path=/; HttpOnly; SameSite=Lax" "$work/page-login.h" || fail "the session cookie's attributes"
code() { curl -sS -o /dev/null -w '%{http_code}' "$@"; }
[ "$(code -b "$work/jar" "$API/api/shell")" = 200 ] || fail "the shell's data with the cookie"
# Changes on the strength of the cookie come only from the site's own pages.
for site in cross-site same-site; do
  [ "$(code -b "$work/jar" -X POST -H "sec-fetch-site: $site" -H 'content-type: application/json' -d '{"password":"x"}' "$API/api/auth/step-up")" = 403 ] \
    || fail "a $site request with the cookie is let through"
done
[ "$(code -b "$work/jar" -X POST -H 'origin: https://evil.example' -H 'content-type: application/json' -d '{"password":"x"}' "$API/api/auth/step-up")" = 403 ] \
  || fail "a request from another origin with the cookie is let through"
cookie=$(awk '$6=="__Host-ferrum" || $6=="ferrum_session" {print $7}' "$work/jar")
[ -n "$cookie" ] || fail "no session cookie in the jar"
[ "$(code -H "authorization: Bearer $cookie" "$API/api/auth/me")" = 401 ] || fail "the cookie's token works as a bearer token"
# App routes: sign-in first; then a page for what is not installed; the core's names are not apps.
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$API/nothing-here/x")" =~ ^30[23]\ .*/login\?next=%2Fnothing-here%2Fx$ ]] || fail "a signed-out app route does not lead to sign-in"
[ "$(code -b "$work/jar" "$API/nothing-here/")" = 404 ] || fail "a route nothing is installed at"
[ "$(code -b "$work/jar" "$API/portal/")" = 404 ] || fail "a name the core keeps"
# The shell's own pages lead a workspace that is not on the shell to today's web app.
[[ "$(curl -sS -o /dev/null -b "$work/jar" -w '%{http_code} %{redirect_url}' "$API/admin/")" =~ ^30[23]\ .*/app/admin$ ]] || fail "the shell's administration for a workspace on the classic interface"
grep -q "unregister" <<<"$(curl -fsS -b "$work/jar" "$API/nothing-here/sw.js")" || fail "the worker of an empty route does not remove itself"
# Apps with interfaces of their own (U4a): Drive, installed through the App Manager, brings its
# interface compiled in; its family, its pages under the bar, and its public page.
consent=$(curl -fsS "${auth[@]}" "$API/api/admin/apps/drive/plan" | json 'd["digest"]') || fail "Drive's install plan"
curl -fsS "${auth[@]}" -H 'content-type: application/json' -d "{\"consent\":\"$consent\"}" "$API/api/admin/apps/drive/install" >/dev/null || fail "installing Drive"
shell=$(curl -fsS -b "$work/jar" "$API/api/shell")
[ "$(json '[a["url"] for a in d["apps"] if a["app"] == "drive"]' <<<"$shell")" = "['/drive/', '/sheets/', '/slides/', '/pdf/']" ] || fail "Drive's launcher entries"
[ "$(json '[f["title"] for f in d["families"]]' <<<"$shell")" = "['Workspace']" ] || fail "Drive's family"
curl -fsS -b "$work/jar" -D "$work/drive.h" -o "$work/drive.html" "$API/drive/" || fail "Drive's page"
grep -q '<ferrum-bar>' "$work/drive.html" && grep -q '"app":{"id":"drive"' "$work/drive.html" || fail "Drive's page lacks the bar or its bootstrap"
curl -sS -D "$work/public.h" -o "$work/public.html" "$API/drive/s/smoke/no-such-link" || fail "Drive's public page"
grep -qi "^x-robots-tag: noindex" "$work/public.h" && grep -qi "^referrer-policy: no-referrer" "$work/public.h" || fail "a public page may be indexed or leak its address"
grep -q '"person":null' "$work/public.html" && ! grep -q '<ferrum-bar>' "$work/public.html" || fail "a public page knows a person or has the bar"
[ "$(code "$API/drive/s/no-such-workspace/x")" = 404 ] || fail "a public page of no workspace"
# Signing out ends the device's session: the cookie no longer works anywhere.
[ "$(code -b "$work/jar" -X POST -H 'sec-fetch-site: same-origin' "$API/api/auth/logout")" = 204 ] || fail "signing out"
[ "$(code -b "$work/jar" "$API/api/shell")" = 401 ] || fail "the cookie still works after signing out"

if [ -n "${FERRUM_EDITORS_IMAGE:-}" ]; then
  step "the browser editors' server"
  EDITORS=$FERRUM_EDITORS_URL
  wait_up "$EDITORS/healthz" "the editor server"
  grep -qi "frame-ancestors ${PUBLIC_URL:-http://localhost:8080}" <<<"$(curl -fsS -D - -o /dev/null "$EDITORS/relay.html")" || fail "the relay may be framed by others"
  curl -fsS -D "$work/editor-headers" -o /dev/null "$EDITORS/gridcraft/index.html" || fail "an editor's page"
  grep -qi "script-src 'self' 'wasm-unsafe-eval' 'sha256-" "$work/editor-headers" || fail "an editor's page allows other scripts"
  [ "$(compose exec -T editors sh -c 'touch /etc/nginx/probe 2>/dev/null && echo writable || echo read-only')" = read-only ] || fail "the editor server can write its root filesystem"
  [ "$(compose exec -T ferrum printenv FERRUM_EDITORS_URL)" = "$EDITORS" ] || fail "the server does not know the editor server"
fi

if [ -n "${SMOKE_MEET:-}" ]; then
  step "the meeting server, and a pass from Ferrum that it accepts"
  MEET="http://127.0.0.1:$FERRUM_MEET_PORT"
  wait_up "$MEET/" "the meeting server"
  [ "$(compose exec -T meet sh -c 'touch /probe 2>/dev/null && echo writable || echo read-only')" = read-only ] || fail "the meeting server can write its root filesystem"
  [ "$(curl -fsS "${auth[@]}" "$API/api/app/comms/meet" | json 'd["available"]')" = True ] || fail "the server does not know the meeting server"
  room=$(curl -fsS "${auth[@]}" -H 'content-type: application/json' -d '{"title":"Smoke"}' "$API/api/app/comms/meet/rooms" | json 'd["id"]') || fail "starting a meeting"
  pass=$(curl -fsS "${auth[@]}" -X POST "$API/api/app/comms/meet/room/$room/token" | json 'd["token"]') || fail "a pass into the meeting"
  [ "$(curl -fsS "$MEET/rtc/validate?access_token=$pass")" = success ] || fail "the meeting server refuses Ferrum's pass"
  forged="${pass%.*}.$(printf '%s' "${pass%.*}" | openssl dgst -sha256 -hmac not-the-secret -binary | base64 | tr '+/' '-_' | tr -d '=')"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' "$MEET/rtc/validate?access_token=$forged")" = 401 ] || fail "the meeting server accepts a pass signed with another secret"
  grep -qi "connect-src 'self' $FERRUM_MEET_URL $MEET;" <<<"$(curl -fsS -D - -o /dev/null "$WEB/app/")" || fail "the web app's pages may not reach the meeting server"

  step "calendar and contacts apps, through the web app, with an app password"
  key=$(curl -fsS "${auth[@]}" -H 'content-type: application/json' -d '{"name":"Smoke phone","scope":"dav"}' "$API/api/auth/api_keys" | json 'd["key"]') || fail "an app password"
  [ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' -X PROPFIND "$WEB/.well-known/caldav")" = "301 $WEB/api/dav/comms/" ] || fail "/.well-known/caldav does not lead to the calendars"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -u "smoke@smoke.test:$key" -X PROPFIND -H 'depth: 0' "$WEB/api/dav/comms/")" = 207 ] || fail "a calendar app cannot sign in"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -u "smoke@smoke.test:wrong" -X PROPFIND "$WEB/api/dav/comms/")" = 401 ] || fail "a wrong app password is let in"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -H "authorization: Bearer $key" "$API/api/auth/me")" = 401 ] || fail "an app password works as an API key"
fi

step "the web app and the API through it"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$WEB/")" =~ ^30[23]\ .*/app/$ ]] || fail "/ does not lead to the app"
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
# Live documents: the handshake of a document's socket (it closes then: no such document).
ws=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "${auth[@]}" -H 'connection: Upgrade' -H 'upgrade: websocket' \
  -H 'sec-websocket-version: 13' -H "sec-websocket-key: $(openssl rand -base64 16)" "$WEB/ws/docs/drive/00000000-0000-0000-0000-000000000000" 2>/dev/null || true)
[ "$ws" = 101 ] || fail "live documents through the web app (HTTP $ws)"

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
