#!/usr/bin/env bash
# Files for the programs beside the server, rehearsed with the server binary and the bundled store on this machine
# (docs/operations.md, "Files for the programs beside the server"; docs/plan/stateless.md): the platform's service
# `object-store` rendered by tasks given DATABASE_URL alone, and SeaweedFS started on the file written by
# deploy/store/start.sh, as deploy/store-files.yml runs it, with servers writing and reading through it.
#
#   scripts/companions_rehearsal.sh ferrum_companions_<name>
#
# 1. `ferrum task services` names the service, its owner, its file and the two secrets the vault makes for it.
# 2. Two renders at once on a fresh database, each into a directory of its own: the vault makes one key pair (one `make`
#    each in its record); the two files are the same; both readable by their owner alone. A render into a directory
#    holding something else is refused and leaves it as it was.
# 3. The file against the vault's record: one credential for each version the record says was rendered, every render
#    naming the service; no key in any task's output or any server's log.
# 4. The store started on the rendered file; a server with its settings in the table and its keys from the vault sets
#    the bucket up; a workspace, a file kept in the store and read back (the store checks every signature).
# 5. A rotation with overlap (`vault_rotate level:service`): the next render gives the store both pairs; the store
#    started again on it; the server that started before (the version before) and one started after (the current) each
#    write and read. The overlap over (its end brought forward): the next render gives the current pair alone, the
#    store started again lets the first server in no more, and the second still.
# 6. A vault that does not open writes nothing: its credential slot removed and its key file not given, a render is
#    refused and the directory stays as it was.
# LiveKit, started on the files a test-declared meeting server renders and called with tokens the vault signs, is
# crates/kernel/tests/companions_it.rs (`livekit-server` on the PATH).
#
# The databases are made and dropped here (their names start with the one given), by the servers' role. Settings:
# FERRUM_BIN (default target/debug/ferrum, built with `cargo build -p ferrum-server --no-default-features --bin ferrum`;
# a debug build, which reads the vault's key file from FERRUM_TEST_RUN_DIR), COMPANIONS_APP_URL (the servers' role,
# which may create databases; default postgres://ferrum:ferrum@localhost:5432), COMPANIONS_PORT (the first of four
# ports from it, and the store's inner ones from it + 10 and + 10010; default 39300). Every process it starts is
# stopped on the way out.
set -euo pipefail
PREFIX=ferrum_companions_
[ $# -eq 1 ] || { echo "usage: $0 ${PREFIX}<name>" >&2; exit 2; }
NAME=$1
[[ "$NAME" == "$PREFIX"* && "$NAME" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $NAME does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
[ -x "$BIN" ] || { echo "no server binary at $BIN (cargo build -p ferrum-server --no-default-features --bin ferrum)" >&2; exit 2; }
APP=${COMPANIONS_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
BASE=${COMPANIONS_PORT:-39300}
A_PORT=$BASE B_PORT=$((BASE + 1)) S3_PORT=$((BASE + 2))
command -v weed >/dev/null || { echo "refused: no weed (SeaweedFS) on the PATH (brew install seaweedfs)" >&2; exit 2; }
for port in "$A_PORT" "$B_PORT" "$S3_PORT" $((BASE + 10)) $((BASE + 11)) $((BASE + 12)) $((BASE + 10010)) $((BASE + 10011)) $((BASE + 10012)); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$port (set COMPANIONS_PORT)" >&2; exit 2; fi
done
work=$(mktemp -d)
A_PID='' B_PID='' STORE_PID=''
DB=${NAME} KEYED=${NAME}_keyed

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$APP/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }
end() { [ -z "$1" ] || { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
cleanup() {
  status=$?
  for p in "$A_PID" "$B_PID" "$STORE_PID"; do end "$p"; done
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -25 "$log"; }; done
  fi
  for db in "$DB" "$KEYED"; do psql "$APP/postgres" -qc "drop database if exists $db with (force)" >/dev/null 2>&1 || true; done
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
for db in "$DB" "$KEYED"; do
  psql "$APP/postgres" -v ON_ERROR_STOP=1 -qc "drop database if exists $db with (force)" -c "create database $db owner ferrum" >/dev/null
done

# A task of this build with the database's address as its one input, as the init container and the one-shot run it.
task() { local db=$1; shift; (cd "$ROOT/server" && exec env -i PATH="$PATH" DATABASE_URL="$APP/$db" "$BIN" -e production task "$@"); }
# A server, its settings in the environment (they are not what is rehearsed here), its keys from the vault.
serve() {
  local port=$1 log=$2
  (cd "$ROOT/server" && exec env -i PATH="$PATH" DATABASE_URL="$APP/$DB" LOG_LEVEL=warn PORT="$port" BINDING=127.0.0.1 \
    PUBLIC_URL="http://127.0.0.1:$port" FERRUM_S3_BUCKET=ferrum FERRUM_S3_ENDPOINT="http://127.0.0.1:$S3_PORT" FERRUM_S3_REGION=us-east-1 \
    FERRUM_S3_SET_UP_BUCKET=true FERRUM_BOOTSTRAP_TOKEN=rehearsal-token FERRUM_FILES_DIR="$work/files" "$BIN" -e production start) >"$work/$log.log" 2>&1 &
  SERVED=$!
  for _ in $(seq 1 120); do
    curl -fsS "http://127.0.0.1:$port/api/ready" >/dev/null 2>&1 && return 0
    kill -0 "$SERVED" 2>/dev/null || return 1
    sleep 1
  done
  fail "the server on $port ($log) neither became ready nor ended"
}
# The bundled store on the rendered keys, as deploy/store-files.yml runs it.
store_up() {
  STORE_DATA="$work/store" STORE_HANDOFF="$work/handoff" STORE_S3_CONFIG="$1/s3.json" STORE_BIND=127.0.0.1 STORE_PORT=$S3_PORT \
    STORE_ARGS="-master.port=$((BASE + 10)) -volume.port=$((BASE + 11)) -filer.port=$((BASE + 12)) -master.volumeSizeLimitMB=64" \
    sh "$ROOT/deploy/store/start.sh" >>"$work/store.log" 2>&1 &
  STORE_PID=$!
  for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:$S3_PORT/healthz" >/dev/null 2>&1 && return 0; sleep 1; done
  fail "the store did not come up"
}
store_down() { end "$STORE_PID"; STORE_PID=''; }
PASSWORD='Rehearsal-Passw0rd'
token() { curl -fsS "$1/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"kept\",\"email\":\"admin@kept.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
# A file of N bytes kept and read back through the server at URL: 0 when it came back the same.
round_trip() {
  local url=$1 tok=$2 n=$3 name
  name="f-$RANDOM.bin"
  head -c "$n" /dev/urandom >"$work/$name"
  local id
  id=$(curl -fsS --max-time 60 "$url/api/files" -H "authorization: Bearer $tok" -F "file=@$work/$name" 2>/dev/null | json 'd["id"]' 2>/dev/null) || return 1
  curl -fsSL --max-time 60 "$url/files/$id" -H "authorization: Bearer $tok" -H 'accept-encoding: gzip' --compressed -o "$work/$name.back" || return 1
  cmp -s "$work/$name" "$work/$name.back"
}
# round_trip until it works: a store just started again takes a few seconds before it writes (its master elects
# itself, its volume server joins). What is refused for its keys is told apart by the server's log, below.
eventually() { for _ in $(seq 1 30); do round_trip "$@" && return 0; sleep 2; done; return 1; }
# How many of a server's requests the store refused for an access key it does not know.
unknown_key() { grep -c "InvalidAccessKeyId" "$work/$1.log" || true; }
# The rendered file's credentials, as `access secret` lines.
credentials() { python3 -c "import json,sys; [print(c['accessKey'], c['secretKey']) for c in json.load(open(sys.argv[1]))['identities'][0]['credentials']]" "$1/s3.json"; }
mode() { stat -f '%Lp' "$1" 2>/dev/null || stat -c '%a' "$1"; }
events() { task "$DB" vault_events format:json limit:1000 2>/dev/null; }

step "1. the services this build declares"
task "$DB" migrate >"$work/migrate.log" 2>&1 || fail "migrate"
task "$DB" services >"$work/services.txt" 2>"$work/services.log" || fail "services"
grep -qx "object-store (platform): s3.json; the vault makes aws_access_key_id, aws_secret_access_key" "$work/services.txt" ||
  fail "the store's service is not listed as declared: $(cat "$work/services.txt")"
sed 's/^/    /' "$work/services.txt"

step "2. two renders at once, each into a directory of its own: one key pair made"
task "$DB" render service:object-store to:"$work/one" >"$work/render-one.log" 2>&1 &
first=$!
task "$DB" render service:object-store to:"$work/two" >"$work/render-two.log" 2>&1 &
second=$!
wait "$first" || fail "the first render"
wait "$second" || fail "the second render"
cmp -s "$work/one/s3.json" "$work/two/s3.json" || fail "two renders at once wrote different keys"
[ "$(mode "$work/one/s3.json")" = 600 ] && [ "$(mode "$work/one")" = 700 ] || fail "the file or its directory is readable by others: $(mode "$work/one/s3.json") $(mode "$work/one")"
[ "$(credentials "$work/one" | wc -l | tr -d ' ')" = 1 ] || fail "a fresh render gives more than one key pair"
grep -qx "s3.json (holds a secret)" "$work/render-one.log" || fail "the render does not name what it wrote: $(cat "$work/render-one.log")"
events >"$work/events.json"
python3 - "$work/events.json" <<'PY' || fail "the vault's record of the renders"
import json, sys
events = json.load(open(sys.argv[1]))
for name in ("aws_access_key_id", "aws_secret_access_key"):
    made = [e for e in events if e["action"] == "make" and e["name"] == name]
    assert len(made) == 1, f"{name} made {len(made)} times"
    d = made[0]["details"]
    assert (d["owner"], d["service"], d["version"]) == ("platform", "object-store", 1), d
    rendered = [e["details"] for e in events if e["action"] == "render" and e["name"] == name]
    assert len(rendered) == 2 and all(r["service"] == "object-store" and r["version"] == 1 for r in rendered), rendered
print("    made once each, rendered twice, every render naming the service")
PY
mkdir -p "$work/theirs" && echo mine >"$work/theirs/notes.txt"
if task "$DB" render service:object-store to:"$work/theirs" >"$work/render-theirs.log" 2>&1; then fail "a render into a directory holding something else"; fi
grep -q "holds notes.txt, which no render of object-store wrote" "$work/render-theirs.log" || fail "the refusal does not say why: $(tail -2 "$work/render-theirs.log")"
[ "$(ls -A "$work/theirs")" = notes.txt ] || fail "the refused render wrote something: $(ls -A "$work/theirs")"

step "3. the server's settings and keys from the database and the vault; the store started on the rendered file"
store_up "$work/one"
serve "$A_PORT" a || fail "server A did not become ready on the store"
A_PID=$SERVED A="http://127.0.0.1:$A_PORT"
curl -fsS "$A/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"kept\",\"name\":\"Kept\",\"admin_email\":\"admin@kept.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null ||
  fail "a workspace"
TA=$(token "$A")
round_trip "$A" "$TA" 70000 || fail "a file kept in the store and read back through A"
anonymous=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$S3_PORT/ferrum/")
[ "$anonymous" = 403 ] || fail "the store answers a request without keys: $anonymous"
echo "    the bucket set up by A with the vault's keys; a file there and back; the store refuses a request without keys"

step "4. a rotation with overlap: both pairs to the store, each server let in"
task "$DB" vault_rotate level:service service:object-store >"$work/rotate.log" 2>&1 || fail "the rotation: $(tail -3 "$work/rotate.log")"
grep -q "platform's aws_secret_access_key is at version 2; the version before is still given to its renders" "$work/rotate.log" || fail "$(cat "$work/rotate.log")"
task "$DB" render service:object-store to:"$work/one" >"$work/render-rotated.log" 2>&1 || fail "the render after the rotation"
[ "$(credentials "$work/one" | wc -l | tr -d ' ')" = 2 ] || fail "the store is not given both pairs during the overlap"
cmp -s <(credentials "$work/two") <(credentials "$work/one" | tail -1) || fail "the pair before is not the first render's"
store_down
store_up "$work/one"
eventually "$A" "$TA" 60000 || fail "A, which started with the pair before, is no longer let in during the overlap"
serve "$B_PORT" b || fail "server B did not become ready"
B_PID=$SERVED B="http://127.0.0.1:$B_PORT"
TB=$(token "$B")
round_trip "$B" "$TB" 50000 || fail "B, started with the current pair, is not let in"
[ "$(unknown_key a)$(unknown_key b)" = 00 ] || fail "a key was refused during the overlap"
echo "    A (the pair before) and B (the current one) each write and read"

step "5. the overlap over: the store started again takes the current pair alone"
sql "$DB" "begin; set local ferrum.vault = 'on'; update vault.secret set valid_until = now() - interval '1 second'
  where owner = 'platform' and name in ('aws_access_key_id', 'aws_secret_access_key') and valid_until is not null; commit;" >/dev/null
task "$DB" render service:object-store to:"$work/one" >"$work/render-over.log" 2>&1 || fail "the render after the overlap"
[ "$(credentials "$work/one" | wc -l | tr -d ' ')" = 1 ] || fail "the pair before is still given after the overlap"
store_down
store_up "$work/one"
eventually "$B" "$TB" 40000 || fail "B, with the current pair, is refused"
if round_trip "$A" "$TA" 40000; then fail "A's pair from before the rotation still opens the store after the overlap"; fi
[ "$(unknown_key a)" -ge 1 ] || fail "A was not refused for its key: $(tail -3 "$work/a.log")"
[ "$(unknown_key b)" = 0 ] || fail "B's current pair was refused"
echo "    A refused for the key it started with, B let in"

step "the record and the logs: names only"
events >"$work/events.json"
python3 - "$work/events.json" "$work/one" "$work/two" "$work" <<'PY' || fail "a key was found where only names belong"
import json, pathlib, sys
events, one, two, work = json.load(open(sys.argv[1])), pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3]), pathlib.Path(sys.argv[4])
keys = set()
for d in (one, two):
    for c in json.loads((d / "s3.json").read_text())["identities"][0]["credentials"]:
        keys |= {c["accessKey"], c["secretKey"]}
rotated = [e for e in events if e["action"] == "rotate"]
assert len(rotated) == 2 and all(e["details"]["by"] == "operator" and e["details"]["service"] == "object-store" for e in rotated), rotated
# Ours: every task's output and every server's log (the store's own log is the store's).
ours = [p for p in [*work.glob("*.log"), *work.glob("*.txt")] if p.name != "store.log"]
text = json.dumps(events) + "".join(p.read_text(errors="ignore") for p in ours)
found = [k[:6] for k in keys if k in text]
assert not found, f"keys found in the record, an output or a log: {found}..."
print(f"    {len(keys)} keys looked for in {len(events)} events and {len(ours)} outputs and logs: none there")
PY

step "6. a vault that does not open writes nothing"
mkdir -p "$work/run"
openssl rand -base64 32 >"$work/run/root.key"
task "$KEYED" migrate >"$work/keyed-migrate.log" 2>&1 || fail "migrate"
(cd "$ROOT/server" && exec env -i PATH="$PATH" DATABASE_URL="$APP/$KEYED" FERRUM_TEST_RUN_DIR="$work/run" "$BIN" -e production task vault_add_slot kind:file) \
  >"$work/keyed-slot.log" 2>&1 || fail "the file slot: $(tail -2 "$work/keyed-slot.log")"
(cd "$ROOT/server" && exec env -i PATH="$PATH" DATABASE_URL="$APP/$KEYED" FERRUM_TEST_RUN_DIR="$work/run" "$BIN" -e production task vault_remove_slot kind:credential) \
  >"$work/keyed-remove.log" 2>&1 || fail "removing the credential slot: $(tail -2 "$work/keyed-remove.log")"
mkdir -p "$work/closed"
if task "$KEYED" render service:object-store to:"$work/closed" >"$work/render-closed.log" 2>&1; then fail "a render with a vault that does not open"; fi
grep -q "the vault" "$work/render-closed.log" || fail "the refusal does not name the vault: $(tail -2 "$work/render-closed.log")"
[ -z "$(ls -A "$work/closed")" ] || fail "a refused render wrote: $(ls -A "$work/closed")"
echo "    refused: $(grep -o 'the vault[^"]*' "$work/render-closed.log" | head -1 | cut -c1-110)"

store_down
end "$A_PID"; A_PID=''
end "$B_PID"; B_PID=''
echo "companions: the store's keys made by the vault, rendered, rotated with an overlap and refused when the vault is shut"
