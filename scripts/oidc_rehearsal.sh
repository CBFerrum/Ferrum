#!/usr/bin/env bash
# Rehearses Ferrum as the sign-in of the programs that run beside it (docs/api.md, "Signing in to programs beside Ferrum";
# docs/plan/stateless.md, D8) with a real relying party on this machine, no Docker: this build's server on a throwaway
# database, with a stand-in for the Cloud app (server/examples/oidc_rehearsal.rs) that registers the forge's companion and
# declares its client from its own code, as Cloud does; and a real Forgejo on SQLite whose OpenID Connect authentication
# source points at the workspace's address, configured as docs/app-contract.md says Cloud must configure it.
#
#   scripts/oidc_rehearsal.sh ferrum_oidcrehearsal_<name>
#
# Everything a person does is asked as a browser asks: cookies kept, redirects followed, Accept-Encoding: gzip.
# 1. Someone signed in to Ferrum opens the forge's sign-in and lands signed in as the account the forge already keeps for
#    them, typing nothing at the forge. Someone without an account there gets one under the name the app gives.
# 2. What the forge is given: an ID token signed with the workspace's key (checked here against jwks_uri with openssl),
#    for the right issuer and audience, naming the subject the forge linked; an access token that reaches the user-info
#    and nothing else of Ferrum's.
# 3. Someone the app does not let in is turned back at the forge, and no account is made for them.
# 4. Leaving: signed out of Ferrum, the access token the forge holds is refused, the forge's sign-in leads to Ferrum's
#    sign-in page, and the app is told (ferrum.companion.signed_out); the forge's own session goes on until the app ends
#    it, which locking the account there does at once. Deactivated: the same, told as access_ended.
#
# Settings: FERRUM_BIN (default ${CARGO_TARGET_DIR:-target}/debug/examples/oidc_rehearsal, built with `cargo build -p
# ferrum-server --no-default-features --example oidc_rehearsal`, as `scripts/local_ci.sh oidc` does), FORGEJO (default
# /opt/homebrew/bin/forgejo), OIDC_APP_URL (the server's role, which may create databases; default
# postgres://ferrum:ferrum@localhost:5432), OIDC_PORT (the server's port, the forge's the next one, the object store's S3 port the one after, its inner ones from it + 10 and + 10010; default 39300). Every
# process it starts is stopped on the way out, and its database dropped.
set -euo pipefail
PREFIX=ferrum_oidcrehearsal
[ $# -eq 1 ] || { echo "usage: $0 ${PREFIX}_<name>" >&2; exit 2; }
NAME=$1
[[ "$NAME" == "$PREFIX"* && "$NAME" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $NAME does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/examples/oidc_rehearsal}")
FORGEJO=${FORGEJO:-/opt/homebrew/bin/forgejo}
APP=${OIDC_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
PORT=${OIDC_PORT:-39300}
F_PORT=$((PORT + 1))
FERRUM="http://127.0.0.1:$PORT"
FORGE="http://localhost:$F_PORT"
[ -x "$BIN" ] || { echo "refused: no $BIN (build it as the header says)" >&2; exit 2; }
[ -x "$FORGEJO" ] || { echo "refused: no Forgejo at $FORGEJO (set FORGEJO)" >&2; exit 2; }
for tool in sqlite3 openssl python3 curl psql weed; do command -v "$tool" >/dev/null || { echo "refused: no $tool on the PATH" >&2; exit 2; }; done
for p in "$PORT" "$F_PORT"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$p (set OIDC_PORT)" >&2; exit 2; fi
done
work=$(mktemp -d)
SERVER_PID='' FORGE_PID='' STORE_PID=''

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# The database as the server's system context reads it (its tables keep to row-level security otherwise).
sql() { psql "$APP/$NAME" -v ON_ERROR_STOP=1 -Atq -c "set ferrum.system = 'on'" -c "$1"; }
forge_sql() { sqlite3 "$work/forge/forgejo.db" "$1"; }
end() { [ -z "$1" ] || { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
cleanup() {
  status=$?
  end "$SERVER_PID"
  end "$FORGE_PID"
  end "$STORE_PID"
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log "$work"/forge/log/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -25 "$log"; }; done
  fi
  psql "$APP/postgres" -qc "drop database if exists $NAME with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
psql "$APP/postgres" -v ON_ERROR_STOP=1 -qc "drop database if exists $NAME with (force)" -c "create database $NAME owner ferrum" >/dev/null
mkdir -p "$work/files"
# A production start keeps files in an object store only: SeaweedFS on this machine (reference/store/start.sh as the
# reference stacks run it), its bucket made over S3.
S_PORT=$((PORT + 2)) S_KEY=FERRUMOIDC S_SECRET=oidc-secret-key-0123456789
STORE_DATA="$work/store" STORE_ACCESS_KEY="$S_KEY" STORE_SECRET_KEY="$S_SECRET" STORE_BIND=127.0.0.1 STORE_PORT=$S_PORT \
  STORE_ARGS="-master.port=$((S_PORT + 10)) -volume.port=$((S_PORT + 11)) -filer.port=$((S_PORT + 12)) -master.volumeSizeLimitMB=64" \
  sh "$ROOT/reference/store/start.sh" >"$work/store.log" 2>&1 &
STORE_PID=$!
for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:$S_PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" --user "$S_KEY:$S_SECRET" "http://127.0.0.1:$S_PORT/ferrum" >/dev/null || fail "making the store's bucket"

# The binary on the database, from the server's directory, as an installation runs it: the vault opens with the
# database's password; nothing else secret is set. The workspace `acme` is the server's one, at 127.0.0.1. Its process
# is the subshell's own (exec), so that a background one's $! is the server, which `end` then stops.
ENVIRONMENT=(LOCO_ENV=production DATABASE_URL="$APP/$NAME" FERRUM_FILES_DIR="$work/files" LOG_LEVEL=warn
  FERRUM_S3_BUCKET=ferrum FERRUM_S3_ENDPOINT="http://127.0.0.1:$S_PORT" FERRUM_S3_REGION=us-east-1 AWS_ACCESS_KEY_ID="$S_KEY" AWS_SECRET_ACCESS_KEY="$S_SECRET"
  FERRUM_DEFAULT_WORKSPACE=acme BINDING=127.0.0.1 PORT="$PORT" PUBLIC_URL="$FERRUM")
on() { (cd "$ROOT/server" && exec env "${ENVIRONMENT[@]}" "$@"); }
# A browser: a cookie jar of its own, redirects followed, gzip accepted. `browse <jar> <curl arguments>`.
browse() { local jar=$1; shift; curl -sS -c "$jar" -b "$jar" -H 'Accept-Encoding: gzip' --compressed "$@"; }
# Signing in to Ferrum from its own page, as login.js does.
sign_in() {
  browse "$1" -o "$work/signed.json" -w '%{http_code}' -H 'Content-Type: application/json' -H 'Sec-Fetch-Site: same-origin' \
    -d "{\"tenant\":\"acme\",\"email\":\"$2\",\"password\":\"Correct-Horse-9\",\"session\":\"cookie\"}" "$FERRUM/api/auth/login"
}
# The forge's sign-in through Ferrum, followed to its end: where it ended and its status.
forge_sign_in() { browse "$1" -L -o "$work/landed.html" -w '%{url_effective} %{http_code}' "$FORGE/user/oauth2/ferrum-acme"; }
# Who the forge says is signed in with this jar (empty: nobody).
forge_who() { browse "$1" "$FORGE/user/settings" | grep -o 'name="name" value="[^"]*"' | sed 's/.*value="\([^"]*\)"/\1/' || true; }
userinfo() { curl -sS -o /dev/null -w '%{http_code}' -H 'Accept-Encoding: gzip' -H "Authorization: Bearer $1" "$FERRUM/oauth/userinfo"; }
# The forge's part of a sign-in, done here as the forge does it, for the person of a browser's jar: their code from the
# authorization endpoint (PKCE, a nonce), redeemed with the client's secret as HTTP Basic credentials. The answer.
tokens_for() {
  local verifier challenge back asked code
  verifier=$(openssl rand -hex 32)
  challenge=$(printf '%s' "$verifier" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  back="$FORGE/user/oauth2/ferrum-acme/callback"
  asked=$(browse "$1" -o /dev/null -w '%{redirect_url}' "$FERRUM/oauth/authorize?response_type=code&client_id=$CLIENT&redirect_uri=$(python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$back")&scope=openid%20profile&state=s&nonce=n-$RANDOM$RANDOM&code_challenge=$challenge&code_challenge_method=S256")
  code=$(python3 -c "import urllib.parse,sys; print(urllib.parse.parse_qs(urllib.parse.urlsplit(sys.argv[1]).query)['code'][0])" "$asked")
  curl -sS -H 'Accept-Encoding: gzip' --compressed -u "$CLIENT:$SECRET" --data-urlencode grant_type=authorization_code --data-urlencode "code=$code" \
    --data-urlencode "redirect_uri=$back" --data-urlencode "code_verifier=$verifier" "$FERRUM/oauth/token"
}
told() {
  for _ in $(seq 1 40); do
    [ "$(sql "select count(*) from outbox where topic = 'ferrum.companion.signed_out' and payload->>'reason' = '$1'")" -ge 1 ] && return 0
    sleep 1
  done
  return 1
}

step "this build's server on a throwaway database, a workspace, and the stand-in's client for the forge"
on "$BIN" task create_tenant slug:acme name:Acme email:admin@acme.test password:Correct-Horse-9 >"$work/tenant.log" 2>&1 || fail "create_tenant"
on "$BIN" setup acme "$FORGE/user/oauth2/ferrum-acme/callback" >"$work/setup.json" 2>"$work/setup.log" || fail "the stand-in's setup"
CLIENT=$(json "d['client_id']" <"$work/setup.json")
SECRET=$(json "d['secret']" <"$work/setup.json")
ADMIN_LOGIN=$(json "d['admin_login']" <"$work/setup.json")
DEV_LOGIN=$(json "d['dev_login']" <"$work/setup.json")
(cd "$ROOT/server" && exec env "${ENVIRONMENT[@]}" "$BIN" start) >"$work/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 180); do curl -fsS "$FERRUM/api/ready" >/dev/null 2>&1 && break; kill -0 "$SERVER_PID" 2>/dev/null || fail "the server ended"; sleep 1; done
curl -fsS -H 'Accept-Encoding: gzip' --compressed "$FERRUM/.well-known/openid-configuration" >"$work/discovery.json" || fail "no discovery document"
[ "$(json "d['issuer']" <"$work/discovery.json")" = "$FERRUM" ] || fail "the issuer is not the workspace's address"
curl -sS -o /dev/null -D "$work/disc.headers" -H 'Accept-Encoding: gzip' "$FERRUM/.well-known/openid-configuration"
grep -qi '^content-encoding: gzip' "$work/disc.headers" || fail "the discovery document is not compressed for a browser"

step "a real Forgejo, its source pointing at the workspace's address, configured as Cloud must"
mkdir -p "$work/forge/custom/conf"
cat >"$work/forge/custom/conf/app.ini" <<EOF
APP_NAME = rehearsal
RUN_MODE = prod
WORK_PATH = $work/forge
[database]
DB_TYPE = sqlite3
PATH = $work/forge/forgejo.db
[server]
HTTP_ADDR = 127.0.0.1
HTTP_PORT = $F_PORT
ROOT_URL = $FORGE/
DISABLE_SSH = true
OFFLINE_MODE = true
[security]
INSTALL_LOCK = true
SECRET_KEY = $(openssl rand -hex 24)
INTERNAL_TOKEN = $(openssl rand -hex 32)
DISABLE_GIT_HOOKS = true
[service]
DISABLE_REGISTRATION = true
REQUIRE_SIGNIN_VIEW = true
[oauth2_client]
ENABLE_AUTO_REGISTRATION = true
USERNAME = preferred_username
ACCOUNT_LINKING = auto
UPDATE_AVATAR = false
[session]
PROVIDER = db
[repository]
ROOT = $work/forge/repos
[log]
MODE = file
LEVEL = Info
ROOT_PATH = $work/forge/log
EOF
F() { "$FORGEJO" --work-path "$work/forge" --config "$work/forge/custom/conf/app.ini" "$@"; }
F migrate >"$work/forge-migrate.log" 2>&1 || fail "forgejo migrate"
FORGE_ADMIN=$(openssl rand -hex 16)
F admin user create --admin --username ferrum --password "$FORGE_ADMIN" --email admin@forge.invalid --must-change-password=false >/dev/null
# The account the forge already keeps for the administrator, as Cloud makes it: their name there, their own address.
F admin user create --username "$ADMIN_LOGIN" --password "$(openssl rand -hex 16)" --email admin@acme.test --must-change-password=false >/dev/null
F admin auth add-oauth --name ferrum-acme --provider openidConnect --key "$CLIENT" --secret "$SECRET" \
  --auto-discover-url "$FERRUM/.well-known/openid-configuration" --scopes openid --scopes profile >"$work/forge-source.log" 2>&1 || fail "the forge's source"
(exec "$FORGEJO" --work-path "$work/forge" --config "$work/forge/custom/conf/app.ini" web) >"$work/forge.log" 2>&1 &
FORGE_PID=$!
for _ in $(seq 1 60); do curl -fsS -o /dev/null "$FORGE/user/login" 2>/dev/null && break; kill -0 "$FORGE_PID" 2>/dev/null || fail "the forge ended"; sleep 1; done
USERS=$(forge_sql "select count(*) from user")

step "1. signed in to Ferrum, the forge's sign-in lands as their own account there, nothing typed at the forge"
A="$work/admin.jar"
[ "$(sign_in "$A" admin@acme.test)" = 200 ] || fail "signing in to Ferrum: $(cat "$work/signed.json")"
landed=$(forge_sign_in "$A")
[ "$landed" = "$FORGE/ 200" ] || fail "the forge's sign-in ended at $landed: $(grep -o '<title>[^<]*' "$work/landed.html")"
[ "$(forge_who "$A")" = "$ADMIN_LOGIN" ] || fail "signed in at the forge as '$(forge_who "$A")', not $ADMIN_LOGIN"
[ "$(forge_sql "select count(*) from user")" = "$USERS" ] || fail "the forge made an account rather than using theirs"
SUB=$(forge_sql "select e.external_id from external_login_user e join user u on u.id = e.user_id where u.lower_name = '$ADMIN_LOGIN'")
[ -n "$SUB" ] || fail "the forge linked no subject to $ADMIN_LOGIN"
D="$work/dev.jar"
[ "$(sign_in "$D" dev@acme.test)" = 200 ] || fail "signing in to Ferrum as dev"
landed=$(forge_sign_in "$D")
[ "$landed" = "$FORGE/ 200" ] || fail "dev's sign-in at the forge ended at $landed"
[ "$(forge_who "$D")" = "$DEV_LOGIN" ] || fail "dev got '$(forge_who "$D")', not the name the app gives, $DEV_LOGIN"

step "2. the ID token is the workspace's, for this issuer and this client; the access token reaches the user-info only"
tokens_for "$A" >"$work/tokens.json"
curl -sS -H 'Accept-Encoding: gzip' --compressed "$FERRUM/oauth/jwks" >"$work/jwks.json"
python3 - "$work" "$FERRUM" "$CLIENT" "$SUB" <<'PY' || fail "the ID token does not check out"
import base64, json, subprocess, sys, time
work, iss, client, sub = sys.argv[1:]
token = json.load(open(f"{work}/tokens.json"))["id_token"]
b64 = lambda s: base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))
head, body, sig = token.split(".")
header, claims = json.loads(b64(head)), json.loads(b64(body))
key = next(k for k in json.load(open(f"{work}/jwks.json"))["keys"] if k["kid"] == header["kid"])
assert header["alg"] == "EdDSA" and key["crv"] == "Ed25519", header
der = bytes.fromhex("302a300506032b6570032100") + b64(key["x"])
open(f"{work}/pub.pem", "w").write("-----BEGIN PUBLIC KEY-----\n" + base64.b64encode(der).decode() + "\n-----END PUBLIC KEY-----\n")
open(f"{work}/signed", "wb").write(f"{head}.{body}".encode())
open(f"{work}/sig", "wb").write(b64(sig))
subprocess.run(["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", f"{work}/pub.pem", "-rawin", "-in", f"{work}/signed", "-sigfile", f"{work}/sig"],
               check=True, capture_output=True)
assert claims["iss"] == iss and claims["aud"] == client and claims["azp"] == client, claims
assert claims["sub"] == sub, f"the subject the forge linked: {claims['sub']} != {sub}"
assert claims["exp"] - claims["iat"] == 300 and claims["exp"] > time.time() and "auth_time" in claims and claims["amr"] == ["password"], claims
print("    signature checked with openssl against jwks_uri; iss, aud, azp, sub, exp, auth_time and amr as they should be")
PY
AT=$(json "d['access_token']" <"$work/tokens.json")
[ "$(userinfo "$AT")" = 200 ] || fail "the user-info refused the access token"
[ "$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $AT" "$FERRUM/api/auth/me")" = 401 ] || fail "the access token reached the API"
FORGE_AT=$(forge_sql "select e.access_token from external_login_user e join user u on u.id = e.user_id where u.lower_name = '$ADMIN_LOGIN'")
[ "$(userinfo "$FORGE_AT")" = 200 ] || fail "the access token the forge keeps does not reach the user-info"

step "3. someone the app does not let in is turned back at the forge, and nothing is made for them"
M="$work/member.jar"
[ "$(sign_in "$M" member@acme.test)" = 200 ] || fail "signing in to Ferrum as member"
landed=$(forge_sign_in "$M" | cut -d' ' -f1)
[ -z "$(forge_who "$M")" ] || fail "member got signed in at the forge as $(forge_who "$M") (ended at $landed)"
[ "$(forge_sql "select count(*) from user where email like 'member%' or lower_name = '$(json "d['member_login']" <"$work/setup.json")'")" = 0 ] || fail "an account was made for member"

step "4. leaving: signed out of Ferrum"
[ "$(browse "$A" -o /dev/null -w '%{http_code}' -X POST -H 'Sec-Fetch-Site: same-origin' "$FERRUM/api/auth/logout")" -lt 300 ] || fail "signing out of Ferrum"
[ "$(userinfo "$FORGE_AT")" = 401 ] || fail "the access token the forge holds still answers"
grep -v 'localhost' "$A" >"$work/no-forge.jar"
again=$(browse "$work/no-forge.jar" -L -o /dev/null -w '%{url_effective}' "$FORGE/user/oauth2/ferrum-acme")
[[ "$again" == "$FERRUM/login?next="* ]] || fail "after signing out, the forge's sign-in went to $again, not Ferrum's sign-in page"
told signed_out || fail "the app was not told (ferrum.companion.signed_out, signed_out)"
[ "$(sql "select payload->>'subject' from outbox where topic = 'ferrum.companion.signed_out' and payload->>'reason' = 'signed_out'")" = "$SUB" ] || fail "told about another subject"
[ "$(forge_who "$A")" = "$ADMIN_LOGIN" ] || fail "the forge's own session ended by itself"
echo "    the forge's own session goes on until the app ends it, as Forgejo keeps it; locking the account ends it:"
curl -fsS -o /dev/null -u "ferrum:$FORGE_ADMIN" -X PATCH -H 'Content-Type: application/json' \
  -d "{\"login_name\":\"$ADMIN_LOGIN\",\"source_id\":0,\"prohibit_login\":true}" "$FORGE/api/v1/admin/users/$ADMIN_LOGIN" || fail "locking the account at the forge"
[ -z "$(forge_who "$A")" ] || fail "a locked account's session still reaches the forge's pages"
echo "    locked: its session is refused at once (what Cloud does on a departure)"

step "4. leaving: deactivated"
# The forge keeps no token for an account it made itself (it knows it by its source and subject): the forge's part, here.
DEV_AT=$(tokens_for "$D" | json "d['access_token']")
[ "$(userinfo "$DEV_AT")" = 200 ] || fail "dev's access token does not answer"
on "$BIN" deactivate acme dev@acme.test >"$work/deactivate.json" 2>"$work/deactivate.log" || fail "deactivating dev"
[ "$(userinfo "$DEV_AT")" = 401 ] || fail "a deactivated person's access token still answers"
told access_ended || fail "the app was not told (access_ended)"
echo "OK: the forge signs people in through Ferrum, and every way of leaving ends it"
