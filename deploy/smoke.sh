#!/usr/bin/env bash
# Deploys the Compose stack (database, Ferrum, PDF renderer) and checks that it works as a
# deployment. A workspace is created, seeded and invoiced by the release being replaced
# (FERRUM_PREVIOUS_IMAGE, when there is one), then the server's container is replaced with
# FERRUM_IMAGE as an upgrade does: the session and the data must survive it. Then, on the new
# image: the security headers; the shell's pages, apps' pages and today's web app's old addresses;
# the API and live updates on the one origin; a Sales Invoice is created, submitted and printed as
# HTML and PDF; the server recovers from a database restart; the
# database and the attachments are backed up, the stack is deleted with its volumes and restored
# from the backups, data, sessions and numbering intact; a short load test runs without errors.
# Everything is removed afterwards. CI runs it on every image it builds. With FERRUM_EDITORS_IMAGE,
# the browser editors' server (editors.yml) runs beside the stack and is checked too; with
# SMOKE_MEET=1, the meeting server (meet.yml), which must accept the passes Ferrum signs, and
# calendar apps signing in with an app password.
#
#   FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:latest deploy/smoke.sh     # needs Docker, curl, openssl, python3 + httpx
set -euo pipefail
cd "$(dirname "$0")"

NEW_IMAGE=${FERRUM_IMAGE:-ghcr.io/cbferrum/ferrum:latest}
# One user drives the load test: lift the per-user budget for it.
export FERRUM_USER_REQUESTS_PER_MINUTE=${FERRUM_USER_REQUESTS_PER_MINUTE:-0}
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-ferrum-smoke}
# The server's port, as the stack publishes it.
export FERRUM_PORT=${SMOKE_PORT:-8080}
export POSTGRES_PASSWORD FERRUM_DB_PASSWORD JWT_SECRET FERRUM_SECRET_KEY
POSTGRES_PASSWORD=$(openssl rand -hex 16)
FERRUM_DB_PASSWORD=$(openssl rand -hex 16)
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
API="http://127.0.0.1:${FERRUM_PORT}"
PASSWORD='Smoke-Test-Passw0rd'

work=$(mktemp -d)
files=(-f docker-compose.yml)
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
    compose logs --tail 200 ferrum pdf db ${FERRUM_EDITORS_IMAGE:+editors} ${SMOKE_MEET:+meet} || true
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

step "starting the stack from $FERRUM_IMAGE"
compose up -d --no-build db ferrum pdf ${FERRUM_EDITORS_IMAGE:+editors} ${SMOKE_MEET:+meet}
wait_up "$API/_health" "the server"

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
# Every workspace is on the shell: Home and the shell's pages, from the interfaces the image carries.
for page in / /admin/ /store/ /account/ /data/; do
  grep -q '<ferrum-bar>' <<<"$(curl -fsS -b "$work/jar" "$API$page")" || fail "the shell's page $page"
done
grep -q "unregister" <<<"$(curl -fsS -b "$work/jar" "$API/nothing-here/sw.js")" || fail "the worker of an empty route does not remove itself"
# Apps with interfaces of their own (U4a): Drive, installed through the App Manager, brings its
# interface compiled in; its family, its pages under the bar, and its public page.
consent=$(curl -fsS "${auth[@]}" "$API/api/admin/apps/drive/plan" | json 'd["digest"]') || fail "Drive's install plan"
curl -fsS "${auth[@]}" -H 'content-type: application/json' -d "{\"consent\":\"$consent\"}" "$API/api/admin/apps/drive/install" >/dev/null || fail "installing Drive"
shell=$(curl -fsS -b "$work/jar" "$API/api/shell")
[ "$(json '[a["url"] for a in d["apps"] if a["app"] == "drive"]' <<<"$shell")" = "['/drive/', '/sheets/', '/slides/', '/pdf/']" ] || fail "Drive's launcher entries"
[ "$(json '[f["title"] for f in d["families"]]' <<<"$shell")" = "['Workspace']" ] || fail "Drive's family"
curl -fsS -b "$work/jar" -D "$work/drive.h" -o "$work/drive.html" "$API/drive/" || fail "Drive's page"
# The bootstrap is JSON in the page (its keys in any order): read it rather than match its text.
bootstrap() { python3 -c 'import json,re,sys; m=re.search(r"<script type=\"application/json\" id=\"ferrum-bootstrap\">(.*?)</script>", sys.stdin.read(), re.S); print(json.dumps(json.loads(m.group(1)) if m else None))'; }
grep -q '<ferrum-bar>' "$work/drive.html" && [ "$(bootstrap <"$work/drive.html" | json 'd["app"]["id"]')" = drive ] || fail "Drive's page lacks the bar or its bootstrap"
curl -sS -D "$work/public.h" -o "$work/public.html" "$API/drive/s/smoke/no-such-link" || fail "Drive's public page"
grep -qi "^x-robots-tag: noindex" "$work/public.h" && grep -qi "^referrer-policy: no-referrer" "$work/public.h" || fail "a public page may be indexed or leak its address"
[ "$(bootstrap <"$work/public.html" | json 'd["person"]')" = None ] && ! grep -q '<ferrum-bar>' "$work/public.html" || fail "a public page knows a person or has the bar"
[ "$(code "$API/drive/s/no-such-workspace/x")" = 404 ] || fail "a public page of no workspace"
install() {
  consent=$(curl -fsS "${auth[@]}" "$API/api/admin/apps/$1/plan" | json 'd["digest"]') || fail "$1's install plan"
  curl -fsS "${auth[@]}" -H 'content-type: application/json' -d "{\"consent\":\"$consent\"}" "$API/api/admin/apps/$1/install" >/dev/null || fail "installing $1"
}
launcher() { curl -fsS -b "$work/jar" "$API/api/shell" | json "[a['url'] for a in d['apps'] if a['app'] == '$1']"; }
# An app without an interface of its own (Forms): its records in the shell, where /open leads.
install forms
[ "$(launcher forms)" = "['/data/forms/']" ] || fail "Forms' records in the launcher"
[ "$(curl -fsS -b "$work/jar" "$API/data/forms/" | bootstrap | json 'd["app"]["scope"]["app"]')" = forms ] || fail "Forms' records page"
[[ "$(curl -sS -o /dev/null -b "$work/jar" -w '%{http_code} %{redirect_url}' "$API/open/Form")" =~ ^30[23]\ .*/data/forms/e/Form$ ]] || fail "/open of Forms' forms"
# An app with one (Helpdesk): its launcher entry and its pages, where /open of its records leads,
# and its old records-only addresses too.
install helpdesk
[ "$(launcher helpdesk)" = "['/helpdesk/']" ] || fail "Helpdesk's launcher entry"
[ "$(curl -fsS -b "$work/jar" "$API/helpdesk/" | bootstrap | json 'd["app"]["id"]')" = helpdesk ] || fail "Helpdesk's page"
[[ "$(curl -sS -o /dev/null -b "$work/jar" -w '%{http_code} %{redirect_url}' "$API/open/Ticket")" =~ ^30[23]\ .*/helpdesk/$ ]] || fail "/open of Helpdesk's tickets"
[[ "$(curl -sS -o /dev/null -b "$work/jar" -w '%{http_code} %{redirect_url}' "$API/data/helpdesk/e/Ticket")" =~ ^30[23]\ .*/helpdesk/e/Ticket$ ]] || fail "Helpdesk's old records address"
# OAuth clients (MCP): the server's consent page, for the person signed in with the cookie.
[[ "$(curl -fsS "$API/.well-known/oauth-authorization-server" | json 'd["authorization_endpoint"]')" == */oauth/authorize ]] || fail "the OAuth authorization endpoint"
client=$(curl -fsS -H 'content-type: application/json' -d '{"client_name":"Smoke","redirect_uris":["http://127.0.0.1/cb"]}' "$API/oauth/register" | json 'd["client_id"]') || fail "registering an OAuth client"
grep -q "Smoke</strong> wants to use Ferrum" <<<"$(curl -fsS -b "$work/jar" "$API/oauth/authorize?response_type=code&client_id=$client&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256")" \
  || fail "the OAuth consent page"
# Signing out ends the device's session: the cookie no longer works anywhere.
[ "$(code -b "$work/jar" -X POST -H 'sec-fetch-site: same-origin' "$API/api/auth/logout")" = 204 ] || fail "signing out"
[ "$(code -b "$work/jar" "$API/api/shell")" = 401 ] || fail "the cookie still works after signing out"

step "the portal (docs/plan/apps-ui.md §9.1)"
# Hosts are an administrator's (custom domains); here straight into the host map, verified.
compose exec -T db psql -qtAU postgres -d ferrum -c "insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at) \
  select h, t.id, 'custom', tg, 'smoke', now() from tenant t, (values ('portal.smoke.test', 'portal')) v(h, tg) \
  where t.slug = 'smoke'" >/dev/null || fail "adding the portal's host"
portal=(-H 'host: portal.smoke.test')
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "${portal[@]}" "$API/")" =~ ^30[23]\ .*/portal/$ ]] || fail "the portal's host leads to the portal"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "${portal[@]}" "$API/helpdesk/")" =~ ^30[23]\ .*/portal/$ ]] || fail "the portal's host serves apps"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "${portal[@]}" "$API/portal/")" =~ ^30[23]\ .*/login\?next=%2Fportal%2F$ ]] || fail "the portal asks to sign in"
grep -q '"codes":true' <<<"$(curl -fsS "${portal[@]}" "$API/login?next=%2Fportal%2F")" || fail "the portal's sign-in offers an emailed code"
# Portal people hold no roles and sign in with emailed codes (this stack sends no email).
[ "$(curl -fsS "${auth[@]}" -H 'content-type: application/json' -d '{"email":"pia@garage.test","full_name":"Pia","kind":"portal"}' "$API/api/admin/users" | json 'd["kind"]')" = portal ] \
  || fail "adding a portal person"
[ "$(code "${auth[@]}" -H 'content-type: application/json' -d '{"email":"x@garage.test","full_name":"X","kind":"portal","roles":["Sales User"]}' "$API/api/admin/users")" = 422 ] \
  || fail "a portal person was given roles"
[ "$(code -H 'content-type: application/json' -d '{"tenant":"smoke","email":"pia@garage.test"}' "$API/api/auth/code")" = 503 ] || fail "emailed codes without a mailer"

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
  # Comms' pages may reach it (installed through the App Manager, with its interface).
  consent=$(curl -fsS "${auth[@]}" "$API/api/admin/apps/comms/plan" | json 'd["digest"]') || fail "Comms' install plan"
  curl -fsS "${auth[@]}" -H 'content-type: application/json' -d "{\"consent\":\"$consent\"}" "$API/api/admin/apps/comms/install" >/dev/null || fail "installing Comms"
  curl -fsS -c "$work/meet-jar" -o /dev/null -H 'content-type: application/json' -H 'sec-fetch-site: same-origin' -d "$page_login" "$API/api/auth/login" || fail "signing in from a page"
  grep -qi "connect-src 'self' $FERRUM_MEET_URL $MEET" <<<"$(curl -fsS -D - -o /dev/null -b "$work/meet-jar" "$API/meet/")" || fail "Meet's pages may not reach the meeting server"

  step "calendar and contacts apps, with an app password"
  key=$(curl -fsS "${auth[@]}" -H 'content-type: application/json' -d '{"name":"Smoke phone","scope":"dav"}' "$API/api/auth/api_keys" | json 'd["key"]') || fail "an app password"
  [ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' -X PROPFIND "$API/.well-known/caldav")" = "301 $API/api/dav/comms/" ] || fail "/.well-known/caldav does not lead to the calendars"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -u "smoke@smoke.test:$key" -X PROPFIND -H 'depth: 0' "$API/api/dav/comms/")" = 207 ] || fail "a calendar app cannot sign in"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -u "smoke@smoke.test:wrong" -X PROPFIND "$API/api/dav/comms/")" = 401 ] || fail "a wrong app password is let in"
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -H "authorization: Bearer $key" "$API/api/auth/me")" = 401 ] || fail "an app password works as an API key"
fi

step "today's web app's old addresses, and live updates"
# /app/ is gone: its addresses lead to their new homes with their query (bookmarks, links in mail,
# the single sign-on return address providers have registered), and its worker removes itself.
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$API/app/e/SalesInvoice/$before?tab=1")" =~ ^30[23]\ .*/open/SalesInvoice/$before\?tab=1$ ]] || fail "an old record address"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$API/app/login?code=c&state=s")" =~ ^30[23]\ .*/login\?code=c\&state=s$ ]] || fail "the single sign-on return address"
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$API/app/")" =~ ^30[23]\ .*/$ ]] || fail "/app/"
grep -q "unregister" <<<"$(curl -fsS "$API/app/sw.js")" || fail "today's web app's worker does not remove itself"
[ "$(curl -fsS "$API/healthz")" = ok ] || fail "/healthz"
# Signed out, Home is the sign-in page first.
[[ "$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$API/")" =~ ^30[23]\ .*/login ]] || fail "/ signed out"
# Live updates: the WebSocket handshake gets through (101); curl then gives up on the open socket.
ws=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "${auth[@]}" -H 'connection: Upgrade' -H 'upgrade: websocket' \
  -H 'sec-websocket-version: 13' -H "sec-websocket-key: $(openssl rand -base64 16)" "$API/ws/live" 2>/dev/null || true)
[ "$ws" = 101 ] || fail "live updates (HTTP $ws)"
# Live documents: the handshake of a document's socket (it closes then: no such document).
ws=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "${auth[@]}" -H 'connection: Upgrade' -H 'upgrade: websocket' \
  -H 'sec-websocket-version: 13' -H "sec-websocket-key: $(openssl rand -base64 16)" "$API/ws/docs/drive/00000000-0000-0000-0000-000000000000" 2>/dev/null || true)
[ "$ws" = 101 ] || fail "live documents (HTTP $ws)"

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
compose up -d --no-build ferrum pdf
wait_up "$API/_health" "the restored server"
[ "$(curl -fsS "${auth[@]}" "$API/api/auth/me" | json 'd["user"]["email"]')" = "smoke@smoke.test" ] || fail "the session after the restore"
[ "$(curl -fsS "${auth[@]}" "$API/api/v1/SalesInvoice/$id" | json 'd["docstatus"]')" = "1" ] || fail "the invoice after the restore"
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
