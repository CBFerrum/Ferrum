#!/usr/bin/env bash
# Rehearses moves with PostgreSQL and the server binary alone, no Docker.
#
# 1. A whole installation (docs/operations.md, Moving an installation): a server on one database with a files directory;
#    a workspace, a record, a file and a webhook whose delivery is still waiting; `ferrum task plan_move`; pg_dump,
#    pg_restore into a second database, the files copied; a server on the copy with the same keys. It checks: no restore
#    hold on the copy, the fence lifted there and still on the source, the waiting delivery made once, sealed values open,
#    the doctor's move checks fine. Then the same without plan_move, which must end in the hold.
# 2. One workspace between two running servers, and back (docs/operations.md, Moving one workspace), then a sandbox of it
#    (A sandbox of a workspace): two servers, each on its own database, files directory and secret key, as two regions
#    that trust each other. `move_workspace` on the second against the first, then `finish_move` on the first and the
#    move back; each is checked from outside, over HTTP: while the run is held still (SIGSTOP) once it has frozen the
#    workspace, the source refuses changes with the moving answer and still reads; afterwards the target serves the
#    records, the file, the API key, the webhook's secret (its deliveries are signed with it), the audit chain, its
#    addresses and a member's Ferrum account, the waiting delivery goes out once, and the source's addresses lead to the
#    target. Then `copy_workspace ... from:here`: the sandbox answers under its own name, signs a person in, and sends
#    nothing, while the live workspace's deliveries still go out.
#
#   scripts/move_rehearsal.sh [--grant] ferrum_moverehearsal_a ferrum_moverehearsal_b
#
# Both databases are created and dropped here, so each name must start with ferrum_moverehearsal_: it can never touch
# another database. The second part turns referential triggers off while a move or a copy runs, so the servers' role
# needs GRANT SET ON PARAMETER session_replication_role for that long. That grant is the whole PostgreSQL server's (other
# test runs grant and revoke it too): with --grant the script grants it right before each move, finish or copy and
# revokes it right after, and on exit whatever happened; without --grant it never grants it, and stops before the first
# move unless the role holds it already. Settings: FERRUM_BIN (the server binary; default target/debug/ferrum, built with
# `cargo build -p ferrum-server --no-default-features --bin ferrum`), MOVE_ADMIN_URL (a superuser, for creating databases,
# the grant and pg_dump, which row-level security would otherwise cut short; default postgres://$USER@localhost:5432),
# MOVE_APP_URL (the servers' role; default postgres://ferrum:ferrum@localhost:5432), MOVE_PORT (default 5391; the second
# server uses the next one, and a webhook receiver the one after).
set -euo pipefail
PREFIX=ferrum_moverehearsal_
GRANT=no
if [ "${1:-}" = --grant ]; then GRANT=yes; shift; fi
[ $# -eq 2 ] || { echo "usage: $0 [--grant] ${PREFIX}a ${PREFIX}b" >&2; exit 2; }
for name in "$1" "$2"; do
  [[ "$name" == "$PREFIX"* && "$name" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $name does not start with $PREFIX" >&2; exit 2; }
done
[ "$1" != "$2" ] || { echo "refused: two different names" >&2; exit 2; }
SOURCE=$1 COPY=$2
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
ADMIN=${MOVE_ADMIN_URL:-postgres://$USER@localhost:5432}
APP=${MOVE_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
ROLE=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.urlparse(sys.argv[1]).username or "")' "$APP")
[[ "$ROLE" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "refused: MOVE_APP_URL names no plain role" >&2; exit 2; }
PORT=${MOVE_PORT:-5391}
COPY_PORT=$((PORT + 1))
HOOK_PORT=$((PORT + 2))
# A server or receiver already on one of the ports would pass for one of these.
for port in "$PORT" "$COPY_PORT" "$HOOK_PORT"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$port (set MOVE_PORT)" >&2; exit 2; fi
done
work=$(mktemp -d)
SERVER_PID='' HOOK_PID='' SOURCE_PID='' A_PID='' B_PID='' MOVE_PID=''

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$ADMIN/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }
fresh() { sql postgres "drop database if exists $1 with (force)" && sql postgres "create database $1 owner $ROLE"; }
revoke() { [ "$GRANT" = yes ] && sql postgres "revoke set on parameter session_replication_role from $ROLE" >/dev/null 2>&1 || true; }
cleanup() {
  status=$?
  [ -n "$MOVE_PID" ] && kill -CONT "$MOVE_PID" 2>/dev/null
  kill $SERVER_PID $HOOK_PID $SOURCE_PID $A_PID $B_PID $MOVE_PID 2>/dev/null || true
  wait 2>/dev/null || true
  revoke
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log "$work"/*.out; do [ -f "$log" ] && { echo "--- $log"; tail -40 "$log"; }; done
  fi
  sql postgres "drop database if exists $SOURCE with (force)" >/dev/null 2>&1 || true
  sql postgres "drop database if exists $COPY with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
if [ "$GRANT" = yes ]; then
  echo "The workspace's move and copy need GRANT SET ON PARAMETER session_replication_role TO $ROLE, a grant of the whole PostgreSQL server: with --grant it is given for each move, finish and copy alone, and taken back after each and on exit."
else
  echo "The workspace's move and copy need GRANT SET ON PARAMETER session_replication_role TO $ROLE, a grant of the whole PostgreSQL server: without --grant this script never gives it, and stops before the first move unless $ROLE holds it already."
fi

# The same keys on both sides, as a move carries them.
export JWT_SECRET FERRUM_SECRET_KEY
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
export LOCO_ENV=development LOG_LEVEL=warn FERRUM_BOOTSTRAP_TOKEN=rehearsal-token
PASSWORD='Rehearsal-Passw0rd'

# Runs the binary on one database and files directory (the development settings: webhooks may reach this machine).
on() { local db=$1 files=$2; shift 2; (cd "$ROOT/server" && DATABASE_URL="$APP/$db" FERRUM_FILES_DIR="$files" "$BIN" "$@"); }
serve() {
  local db=$1 files=$2 port=$3
  (cd "$ROOT/server" && DATABASE_URL="$APP/$db" FERRUM_FILES_DIR="$files" PORT=$port BINDING=127.0.0.1 PUBLIC_URL="http://127.0.0.1:$port" \
    exec "$BIN" start) >>"$work/$db.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 120); do
    curl -fsS "http://127.0.0.1:$port/api/ready" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server on $db did not come up"
}
end() { [ -z "$1" ] || { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
token() { curl -fsS "$1/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"moving\",\"email\":\"admin@moving.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
contact() { curl -sS -o /dev/null -w '%{http_code}' "$1/api/v1/Contact" -H "authorization: Bearer $2" -H 'content-type: application/json' -d "{\"first_name\":\"$3\"}"; }
# Deliveries the receiver got for a contact's creation, by its first name.
received() { if [ -f "$work/hooks.jsonl" ]; then grep -c "\"first_name\": *\"$1\"" "$work/hooks.jsonl" || true; else echo 0; fi; }
receive() {
  python3 - "$HOOK_PORT" "$work/hooks.jsonl" >"$work/hooks.log" 2>&1 <<'PY' &
import http.server, sys
port, out = int(sys.argv[1]), sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(out, "ab") as f:
            f.write(body.replace(b"\n", b" ") + b"\n")
        self.send_response(200); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
  HOOK_PID=$!
  sleep 1
}

step "the source: $SOURCE, with a workspace, a file and a webhook that cannot be delivered yet"
fresh "$SOURCE"
fresh "$COPY"
mkdir -p "$work/files-a" "$work/files-b"
serve "$SOURCE" "$work/files-a" "$PORT"
SOURCE_PID=$SERVER_PID
A="http://127.0.0.1:$PORT"
curl -fsS "$A/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"moving\",\"name\":\"Moving\",\"admin_email\":\"admin@moving.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
TA=$(token "$A")
curl -fsS "$A/api/admin/webhooks" -H "authorization: Bearer $TA" -H 'content-type: application/json' \
  -d "{\"name\":\"rehearsal\",\"url\":\"http://127.0.0.1:$HOOK_PORT/hook\",\"topics\":[\"*\"]}" >/dev/null
FILE=$(curl -fsS "$A/api/files" -H "authorization: Bearer $TA" -F "file=@$ROOT/README.md" | json 'd["url"]')
# The receiver is not up: the delivery fails, and waits for its next try.
[ "$(contact "$A" "$TA" Before)" = 201 ] || fail "a contact on the source"
for _ in $(seq 1 30); do
  [ "$(sql "$SOURCE" "select count(*) from webhook_delivery where status = 'pending' and attempts > 0")" -gt 0 ] && break
  sleep 1
done
[ "$(sql "$SOURCE" "select count(*) from webhook_delivery where status = 'pending' and attempts > 0")" -gt 0 ] || fail "no delivery waiting"

step "plan_move on the source"
on "$SOURCE" "$work/files-a" task plan_move reason:"rehearsal" wait:0 | tee "$work/plan.out"
grep -q "Start ONE copy" "$work/plan.out" || fail "plan_move does not warn about two copies"
[ "$(contact "$A" "$TA" Fenced)" = 503 ] || fail "the fenced source took a change"
LOG_LEVEL=error on "$SOURCE" "$work/files-a" task doctor format:json >"$work/source-doctor.json" || true
python3 - "$work/source-doctor.json" <<'PY'
import json, sys
text = open(sys.argv[1]).read()
found = {f["check"]: f for f in json.loads(text[text.index("\n[") + 1 if not text.startswith("[") else 0:])}
assert found["restore"]["level"] == "ok", found["restore"]
assert found["move"]["level"] == "blocker" and "source of a planned move" in found["move"]["detail"], found["move"]
PY

step "dump, restore into $COPY, copy the files"
pg_dump -Fc -f "$work/move.dump" "$ADMIN/$SOURCE"
pg_restore --exit-on-error -d "$ADMIN/$COPY" "$work/move.dump"
cp -R "$work/files-a/." "$work/files-b/"

step "the copy: no hold, takes changes, delivers what was waiting once"
receive
serve "$COPY" "$work/files-b" "$COPY_PORT"
B="http://127.0.0.1:$COPY_PORT"
[ -z "$(sql "$COPY" "select held_since from platform_incarnation")" ] || fail "the copy is held"
[ "$(sql "$COPY" "select count(*) from platform_move")" = 0 ] || fail "the mark is still on the copy"
[ "$(sql "$COPY" "select count(*) from audit_event where action = 'platform.move_adopted'")" = 1 ] || fail "no record of the move"
TB=$(token "$B")
[ "$(contact "$B" "$TB" After)" = 201 ] || fail "the copy took no change"
[ "$(contact "$A" "$TA" Fenced)" = 503 ] || fail "the source is no longer fenced"
for _ in $(seq 1 90); do
  [ "$(received Before)" -ge 1 ] && [ "$(received After)" -ge 1 ] && break
  sleep 1
done
sleep 5
[ "$(received Before)" = 1 ] || fail "what was waiting went out $(received Before) times"
[ "$(received After)" = 1 ] || fail "what came after went out $(received After) times"
curl -fsS -o /dev/null "$B$FILE" -H "authorization: Bearer $TB" || fail "the file is not on the copy"

curl -fsS "$B/metrics" | grep -q 'ferrum_http_request_duration_seconds_count{app="other",class="records"}' || fail "no request metrics"

step "the doctor and check_files on the copy"
LOG_LEVEL=error on "$COPY" "$work/files-b" task doctor format:json >"$work/doctor.json" || true
python3 - "$work/doctor.json" <<'PY'
import json, sys
text = open(sys.argv[1]).read()
found = {f["check"]: f["level"] for f in json.loads(text[text.index("\n[") + 1 if not text.startswith("[") else 0:])}
for check in ["secrets.unopened", "files.missing", "restore"]:
    assert found.get(check) == "ok", (check, found.get(check))
assert "move" not in found, found["move"]
PY
on "$COPY" "$work/files-b" task check_files | tail -1
end "$SERVER_PID"; end "$HOOK_PID" # the copy, and the receiver: what comes next cannot be delivered

step "without plan_move: the copy is held"
on "$SOURCE" "$work/files-a" task cancel_move reason:"rehearsing the other way" >/dev/null
[ "$(contact "$A" "$TA" Unplanned)" = 201 ] || fail "the source takes changes again"
for _ in $(seq 1 30); do
  [ "$(sql "$SOURCE" "select count(*) from webhook_delivery where status = 'pending' and payload::text like '%Unplanned%'")" -gt 0 ] && break
  sleep 1
done
end "$SOURCE_PID" # it would deliver once the receiver is back
pg_dump -Fc -f "$work/unplanned.dump" "$ADMIN/$SOURCE"
fresh "$COPY"
pg_restore --exit-on-error -d "$ADMIN/$COPY" "$work/unplanned.dump"
receive
serve "$COPY" "$work/files-b" "$COPY_PORT"
[ -n "$(sql "$COPY" "select held_since from platform_incarnation")" ] || fail "an unplanned copy is not held"
[ "$(sql "$COPY" "select count(*) from webhook_delivery where status = 'dead' and payload::text like '%Unplanned%'")" -ge 1 ] || fail "what was waiting is not set aside"
sleep 5
[ "$(received Unplanned)" = 0 ] || fail "a held copy delivered"
end "$SERVER_PID"; end "$HOOK_PID"
SERVER_PID='' HOOK_PID='' SOURCE_PID=''
echo "the installation's move passed"

# --- One workspace between two running servers, and back; then a sandbox ------------------------------------------
# Each server its own database, files and keys, as regions a and b that trust each other, each with a base domain of
# its own (*.localhost names reach this machine; requests carry the host in a Host header).
A_PORT=$PORT B_PORT=$COPY_PORT
# The development settings without a mailer, as a server is by default in production (MAILER_ENABLED=false): a Ferrum
# account is then made at once from a workspace's password, where a mailer would mail a code to prove the address first.
mkdir -p "$work/config"
python3 - "$ROOT/server/config/development.yaml" "$work/config/development.yaml" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
open(sys.argv[2], "w").write(re.sub(r"^mailer:\n(?:[ \t]+.*\n|\n)*", "", text, flags=re.M))
PY
grep -q '^mailer:' "$work/config/development.yaml" && fail "the settings without a mailer still have one"
key() { openssl rand -base64 "$1" | tr -d '\n'; }
A_ENV=(LOCO_CONFIG_FOLDER="$work/config" DATABASE_URL="$APP/$SOURCE" FERRUM_FILES_DIR="$work/wfiles-a" JWT_SECRET="$(key 48)" FERRUM_SECRET_KEY="$(key 32)" FERRUM_REGION=a
  FERRUM_REGIONS="b=http://accounts.b.localhost:$B_PORT" FERRUM_BASE_DOMAIN=a.localhost PORT=$A_PORT BINDING=127.0.0.1 PUBLIC_URL="http://a.localhost:$A_PORT")
B_ENV=(LOCO_CONFIG_FOLDER="$work/config" DATABASE_URL="$APP/$COPY" FERRUM_FILES_DIR="$work/wfiles-b" JWT_SECRET="$(key 48)" FERRUM_SECRET_KEY="$(key 32)" FERRUM_REGION=b
  FERRUM_REGIONS="a=http://accounts.a.localhost:$A_PORT" FERRUM_BASE_DOMAIN=b.localhost PORT=$B_PORT BINDING=127.0.0.1 PUBLIC_URL="http://b.localhost:$B_PORT")
side_env() { if [ "$1" = a ]; then printf '%s\n' "${A_ENV[@]}"; else printf '%s\n' "${B_ENV[@]}"; fi; }
side_get() { side_env "$1" | sed -n "s/^$2=//p"; }
port_of() { if [ "$1" = a ]; then echo "$A_PORT"; else echo "$B_PORT"; fi; }
# A server of side a or b; its pid in A_PID or B_PID.
wserve() {
  local side=$1 port
  port=$(port_of "$side")
  local envs=()
  while IFS= read -r line; do envs+=("$line"); done < <(side_env "$side")
  (cd "$ROOT/server" && exec env "${envs[@]}" "$BIN" start) >>"$work/server-$side.log" 2>&1 &
  if [ "$side" = a ]; then A_PID=$!; else B_PID=$!; fi
  for _ in $(seq 1 120); do curl -fsS "http://127.0.0.1:$port/api/ready" >/dev/null 2>&1 && return 0; sleep 1; done
  fail "server $side did not come up"
}
# A task on side a or b, with more settings given before the task's arguments.
wtask() {
  local side=$1; shift
  local envs=()
  while IFS= read -r line; do envs+=("$line"); done < <(side_env "$side")
  (cd "$ROOT/server" && exec env "${envs[@]}" "$@")
}
# A request to side a or b as to a host of it: `at a moving.a.localhost /api/... [curl arguments]`.
at() { local side=$1 host=$2 path=$3 port; shift 3; port=$(port_of "$side"); curl -sS -H "Host: $host:$port" "http://127.0.0.1:$port$path" "$@"; }
status() { local side=$1 host=$2 path=$3; shift 3; at "$side" "$host" "$path" -o "$work/body" -w '%{http_code}' "$@"; }
# A page as a signed-out browser gets it: its status and where it leads.
page() { at "$1" "$2" "$3" -o /dev/null -w '%{http_code} %{redirect_url}'; }
# JSON bodies are made apart: bash 3.2 (macOS) mangles escaped quotes inside a command substitution inside quotes.
signin() { printf '{"tenant":"%s","email":"%s","password":"%s"}' "${1:-moving}" "${2:-admin@moving.test}" "$PASSWORD"; }
# Pat's Ferrum account: their workspace password opens nothing on a workspace's host once their account has joined one,
# and the refusal names the accounts host where they sign in instead (here, or a visiting account's region).
ferrum_account() {
  local side=$1 body
  body=$(signin moving pat@moving.test)
  [ "$(status "$side" "moving.$side.localhost" /api/auth/login -H 'content-type: application/json' -d "$body")" = 403 ] \
    && grep -q "signs in with its Ferrum account, at http://accounts.$side.localhost" "$work/body" \
    || fail "pat's workspace account at $side is not joined to a Ferrum account there: $(cat "$work/body")"
}
wlogin() { local body; body=$(signin "${3:-moving}"); at "$1" "$2" /api/auth/login -H 'content-type: application/json' -d "$body" | json 'd["token"]'; }
wcontact() { local body; body=$(printf '{"first_name":"%s"}' "$4"); status "$1" "$2" /api/v1/Contact -H "authorization: Bearer $3" -H 'content-type: application/json' -d "$body"; }
wnames() { at "$1" "$2" '/api/v1/Contact?limit=200' -H "authorization: Bearer $3" | json '" ".join(sorted(r["first_name"] for r in d["rows"]))'; }
grant() {
  if [ "$GRANT" = yes ]; then sql postgres "grant set on parameter session_replication_role to $ROLE" >/dev/null; fi
  [ "$(sql postgres "select has_parameter_privilege('$ROLE', 'session_replication_role', 'SET')")" = t ] \
    || fail "$ROLE lacks GRANT SET ON PARAMETER session_replication_role (run with --grant to have it given for each step)"
}
# The receiver: each delivery's signature and body, one JSON line each.
hooks() {
  python3 - "$HOOK_PORT" "$work/signed.jsonl" >"$work/hooks.log" 2>&1 <<'PY' &
import http.server, json, sys
port, out = int(sys.argv[1]), sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(out, "a") as f:
            f.write(json.dumps({"sig": self.headers.get("x-ferrum-signature", ""), "body": body.decode()}) + "\n")
        self.send_response(200); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
  HOOK_PID=$!
  sleep 1
}
# Deliveries of the contact named $1; with a secret, every one of them must carry a signature made with it.
delivered() {
  python3 - "$work/signed.jsonl" "$1" "${2:-}" <<'PY'
import hashlib, hmac, json, os, re, sys
path, name, secret = sys.argv[1], sys.argv[2], sys.argv[3]
lines = [json.loads(l) for l in open(path)] if os.path.exists(path) else []
mine = [l for l in lines if re.search(r'"first_name":\s*"%s"' % re.escape(name), l["body"])]
for l in mine if secret else []:
    parts = dict(p.split("=", 1) for p in l["sig"].split(","))
    want = hmac.new(secret.encode(), (parts["t"] + "." + l["body"]).encode(), hashlib.sha256).hexdigest()
    if not hmac.compare_digest(want, parts.get("v1", "")):
        print("unsigned"); sys.exit()
print(len(mine))
PY
}
# Waits until the contact named $1 has been delivered once, then a little longer, and says how many times.
once() { for _ in $(seq 1 90); do [ "$(delivered "$1")" -ge 1 ] && break; sleep 1; done; sleep 4; delivered "$1" "$2"; }
# A delivery left waiting at side $1: the receiver is down when the contact named $2 is made, and its first try fails.
waiting_delivery() {
  local side=$1 host=$2 token=$3 name=$4 db
  db=$(side_get "$side" DATABASE_URL); db=${db##*/}
  [ "$(wcontact "$side" "$host" "$token" "$name")" = 201 ] || fail "the contact $name"
  for _ in $(seq 1 30); do
    [ "$(sql "$db" "select count(*) from webhook_delivery where status = 'pending' and attempts > 0 and payload::text like '%$name%'")" -gt 0 ] && return 0
    sleep 1
  done
  fail "no delivery of $name waits at $side"
}

# Moves the workspace from side $1 to side $2 with move_workspace on $2. The run is held still once it has frozen the
# workspace at the source, and the source is asked meanwhile: changes refused with the moving answer, reading served.
move() {
  local from=$1 to=$2 token=$3 from_host="moving.$1.localhost"
  local envs=()
  while IFS= read -r line; do envs+=("$line"); done < <(side_env "$to")
  envs+=(FERRUM_MOVE_SOURCE_DATABASE_URL="$(side_get "$from" DATABASE_URL)" FERRUM_MOVE_SOURCE_SECRET_KEYS="$(side_get "$from" FERRUM_SECRET_KEY)"
    FERRUM_MOVE_SOURCE_FILES_DIR="$(side_get "$from" FERRUM_FILES_DIR)" FERRUM_MOVE_SOURCE_REGION="$from")
  grant
  (cd "$ROOT/server" && exec env "${envs[@]}" "$BIN" task move_workspace tenant:moving wait:1) >"$work/move-$from-$to.out" 2>&1 &
  MOVE_PID=$!
  for _ in $(seq 1 1200); do
    grep -q "is frozen at the source" "$work/move-$from-$to.out" && break
    kill -0 "$MOVE_PID" 2>/dev/null || break
    sleep 0.05
  done
  kill -STOP "$MOVE_PID" 2>/dev/null || fail "the move ended before it froze the workspace: $(cat "$work/move-$from-$to.out")"
  grep -q "is frozen at the source" "$work/move-$from-$to.out" || fail "the move did not freeze the workspace"
  local code answer
  code=$(wcontact "$from" "$from_host" "$token" "During-$from")
  answer=$(json 'd["message"]' <"$work/body" 2>/dev/null || cat "$work/body")
  [ "$code" = 503 ] && [[ "$answer" == *moving* ]] || fail "the frozen source took a change: $code $answer"
  at "$from" "$from_host" /api/v1/Contact -X POST -D - -o /dev/null -H "authorization: Bearer $token" -H 'content-type: application/json' \
    -d '{"first_name":"During"}' | grep -qi '^retry-after:' || fail "the moving answer has no Retry-After"
  [ "$(status "$from" "$from_host" '/api/v1/Contact?limit=5' -H "authorization: Bearer $token")" = 200 ] || fail "the frozen source does not read"
  echo "    during the move: $from refused a change (503: $answer) and read"
  kill -CONT "$MOVE_PID"
  wait "$MOVE_PID" || { MOVE_PID=''; fail "move_workspace from $from to $to failed"; }
  MOVE_PID=''
  revoke
  sed 's/^/    /' "$work/move-$from-$to.out"
}
# What the target serves afterwards, and what the source answers.
arrived() {
  local from=$1 to=$2 key=$3 secret=$4 queued=$5 host="moving.$2.localhost" t names location h
  t=$(wlogin "$to" "$host")
  names=$(wnames "$to" "$host" "$t")
  for n in Before-1 "$queued"; do [[ " $names " == *" $n "* ]] || fail "$to does not have $n ($names)"; done
  [ "$(at "$to" "$host" "$FILE" -H "authorization: Bearer $t" | shasum -a 256 | cut -d' ' -f1)" = "$FILE_SUM" ] || fail "the file's bytes differ at $to"
  [ "$(status "$to" "$host" '/api/v1/Contact?limit=1' -H "authorization: Bearer $key")" = 200 ] || fail "the API key does not work at $to"
  [ "$(at "$to" "$host" "/api/v1/Contact/$BEFORE/audit" -H "authorization: Bearer $t" | json 'd["chain_valid"]')" = True ] || fail "the audit chain does not verify at $to"
  # Its addresses are its own here: their sign-in page hands over to this region's accounts host, for that address.
  # (A moved address sends on to the new one instead, and one let go of, nowhere.)
  for h in "$host" "sales--moving.$to.localhost"; do
    location=$(page "$to" "$h" /login)
    [[ "$location" == "303 http://accounts.$to.localhost:$(port_of "$to")/authorize?host=$h%3A$(port_of "$to")&"* ]] \
      || fail "$h at $to answers $location, not the workspace's sign-in"
  done
  hooks
  [ "$(once "$queued" "$secret")" = 1 ] || fail "the waiting delivery of $queued went out $(delivered "$queued") times, or unsigned"
  [ "$(wcontact "$to" "$host" "$t" "After-$to")" = 201 ] || fail "$to took no change"
  [ "$(once "After-$to" "$secret")" = 1 ] || fail "a new delivery from $to went out $(delivered "After-$to") times, or unsigned"
  for h in "moving.$from.localhost" "sales--moving.$from.localhost"; do
    location=$(page "$from" "$h" /login)
    [[ "$location" == "308 http://moving.$to.localhost:"* ]] || fail "$h at $from answers $location, not a move to moving.$to.localhost"
  done
  echo "    at $to: records, the file's bytes, the API key, the audit chain, sales--moving.$to.localhost; the waiting delivery once, signed with the webhook's secret; $from leads to moving.$to.localhost"
}

step "the workspace at a: records, a file, a webhook whose delivery waits, an API key, a name under the base, a Ferrum account"
end "$HOOK_PID"; HOOK_PID=''
fresh "$SOURCE"
fresh "$COPY"
rm -f "$work/signed.jsonl"
mkdir -p "$work/wfiles-a" "$work/wfiles-b"
wserve a
wserve b
curl -fsS "http://127.0.0.1:$A_PORT/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"moving\",\"name\":\"Moving\",\"admin_email\":\"admin@moving.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
TA=$(wlogin a moving.a.localhost)
[ "$(wcontact a moving.a.localhost "$TA" Before-1)" = 201 ] || fail "a contact at a"
BEFORE=$(json 'd["id"]' <"$work/body")
FILE=$(at a moving.a.localhost /api/files -H "authorization: Bearer $TA" -F "file=@$ROOT/README.md" | json 'd["url"]')
FILE_SUM=$(shasum -a 256 <"$ROOT/README.md" | cut -d' ' -f1)
KEY=$(at a moving.a.localhost /api/auth/api_keys -H "authorization: Bearer $TA" -H 'content-type: application/json' -d '{"name":"rehearsal"}' | json 'd["key"]')
[ "$(status a moving.a.localhost /api/admin/domains -H "authorization: Bearer $TA" -H 'content-type: application/json' -d '{"name":"sales"}')" = 201 ] \
  || fail "a name under the base: $(cat "$work/body")"
# Pat, a member, makes their Ferrum account at a: the first sign-in at the accounts host (from its page) makes it from the
# workspace's password. The administrator keeps signing in with the workspace's own, as these checks do.
PAT=$(printf '{"email":"pat@moving.test","full_name":"Pat","password":"%s","roles":[],"is_admin":false}' "$PASSWORD")
[ "$(status a moving.a.localhost /api/admin/users -H "authorization: Bearer $TA" -H 'content-type: application/json' -d "$PAT")" = 201 ] \
  || fail "a member: $(cat "$work/body")"
ACCOUNT=$(printf '{"email":"pat@moving.test","password":"%s"}' "$PASSWORD")
[ "$(status a accounts.a.localhost /api/accounts/sign-in -H 'sec-fetch-site: same-origin' -H 'content-type: application/json' -d "$ACCOUNT")" = 200 ] \
  || fail "pat's Ferrum account at a: $(cat "$work/body")"
ferrum_account a
HOOK=$(printf '{"name":"rehearsal","url":"http://127.0.0.1:%s/hook","topics":["*"]}' "$HOOK_PORT")
SECRET=$(at a moving.a.localhost /api/admin/webhooks -H "authorization: Bearer $TA" -H 'content-type: application/json' -d "$HOOK" | json 'd["secret"]')
waiting_delivery a moving.a.localhost "$TA" Queued-a

step "move_workspace at b, from a"
move a b "$TA"
grep -q "1 members keep their Ferrum account here" "$work/move-a-b.out" || fail "the member's Ferrum account did not come along as a visiting account"
arrived a b "$KEY" "$SECRET" Queued-a
ferrum_account b
end "$HOOK_PID"; HOOK_PID=''

step "finish_move at a, then the move back to a"
grant
wtask a "$BIN" task finish_move tenant:moving note:"checked at b" | tail -1
revoke
TB=$(wlogin b moving.b.localhost)
waiting_delivery b moving.b.localhost "$TB" Queued-b
move b a "$TB"
grep -q "1 members keep their Ferrum account here" "$work/move-b-a.out" || fail "the member did not get their own Ferrum account back at a"
arrived b a "$KEY" "$SECRET" Queued-b
ferrum_account a

step "copy_workspace from:here at a: the sandbox answers under its own name, and sends nothing"
grant
wtask a "$BIN" task copy_workspace tenant:moving as:moving-try until:"$(python3 -c 'import datetime; print(datetime.date.today() + datetime.timedelta(days=7))')" from:here | tail -2
revoke
TS=$(wlogin a moving-try.a.localhost moving-try)
[ -n "$TS" ] || fail "nobody signs in to the sandbox"
names=$(wnames a moving-try.a.localhost "$TS")
[[ " $names " == *" Before-1 "* ]] || fail "the sandbox does not hold the workspace's records ($names)"
[ "$(wcontact a moving-try.a.localhost "$TS" Sandbox-1)" = 201 ] || fail "the sandbox took no change"
TA=$(wlogin a moving.a.localhost)
[ "$(wcontact a moving.a.localhost "$TA" Live-1)" = 201 ] || fail "the live workspace took no change"
[ "$(once Live-1 "$SECRET")" = 1 ] || fail "the live workspace's delivery went out $(delivered Live-1) times"
[ "$(delivered Sandbox-1)" = 0 ] || fail "the sandbox delivered"
echo "    the sandbox moving-try signs in and holds the records; its change went nowhere, the live workspace's went out once"
echo "the move rehearsal passed"
