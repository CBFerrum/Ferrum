#!/usr/bin/env bash
# Rehearses moving a whole installation (docs/operations.md, Moving an installation) with PostgreSQL and the server binary
# alone, no Docker: a server on one database with a files directory; a workspace, a record, a file and a webhook whose
# delivery is still waiting; `ferrum task plan_move`; pg_dump, pg_restore into a second database, the files copied; a
# server on the copy with the same keys. It checks: no restore hold on the copy, the fence lifted there and still on the
# source, the waiting delivery made once, sealed values open, the doctor's move checks fine. Then the same without
# plan_move, which must end in the hold.
#
#   scripts/move_rehearsal.sh ferrum_moverehearsal_a ferrum_moverehearsal_b
#
# Both databases are created and dropped here, so each name must start with ferrum_moverehearsal_: it can never touch
# another database. Settings: FERRUM_BIN (the server binary; default target/debug/ferrum, built with
# `cargo build -p ferrum-server --no-default-features --bin ferrum`), MOVE_ADMIN_URL (a superuser, for creating databases
# and for pg_dump, which row-level security would otherwise cut short; default postgres://$USER@localhost:5432),
# MOVE_APP_URL (the server's role; default postgres://ferrum:ferrum@localhost:5432), MOVE_PORT (default 5391; the copy
# uses the next one).
set -euo pipefail
PREFIX=ferrum_moverehearsal_
[ $# -eq 2 ] || { echo "usage: $0 ${PREFIX}a ${PREFIX}b" >&2; exit 2; }
for name in "$1" "$2"; do
  [[ "$name" == "$PREFIX"* && "$name" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $name does not start with $PREFIX" >&2; exit 2; }
done
[ "$1" != "$2" ] || { echo "refused: two different names" >&2; exit 2; }
SOURCE=$1 COPY=$2
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
ADMIN=${MOVE_ADMIN_URL:-postgres://$USER@localhost:5432}
APP=${MOVE_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
PORT=${MOVE_PORT:-5391}
COPY_PORT=$((PORT + 1))
HOOK_PORT=$((PORT + 2))
work=$(mktemp -d)
SERVER_PID='' HOOK_PID='' SOURCE_PID=''

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$ADMIN/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }
fresh() { sql postgres "drop database if exists $1 with (force)" && sql postgres "create database $1 owner ferrum"; }
cleanup() {
  status=$?
  kill $SERVER_PID $HOOK_PID $SOURCE_PID 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -40 "$log"; }; done
  fi
  sql postgres "drop database if exists $SOURCE with (force)" >/dev/null 2>&1 || true
  sql postgres "drop database if exists $COPY with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

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
end() { [ -n "$1" ] && { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
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
echo "the move rehearsal passed"
