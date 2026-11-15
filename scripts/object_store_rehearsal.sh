#!/usr/bin/env bash
# Rehearses object storage only (docs/plan/stateless.md, D6; docs/operations.md, "Object storage") with PostgreSQL, the
# server binary and real S3 servers on this machine, no Docker: SeaweedFS run by deploy/store/start.sh as Compose runs
# it (the store checks every signature), and moto 5.0.28.
#
#   scripts/object_store_rehearsal.sh ferrum_objrehearsal_<name>
#
# 1. Refused on a directory: production settings, a new database, no bucket: the server does not start, and says what
#    to set.
# 2. Moving files: an installation keeping its files in a directory (made in development) starts in production, warned,
#    and the doctor's line about it is error-level. The bundled store starts (its keys made and handed over); two servers
#    set to it read and write, each reading what the other wrote and the directory's files; the server keeps the store's
#    keys in the vault and deletes the file that handed them over. `ferrum task files_move` is killed partway and run
#    again: it goes on, and records that the directory is no longer read; restarted, with the directory moved away, both
#    servers serve every file, each as its row says.
# 3. The doctor's lines on moto: versioning, a lifecycle rule for old versions and encryption at rest, bad, then good
#    once each is switched on; moto takes requests without keys, and the doctor says the bucket is public.
# 4. The drill: a dump at a moment T; after it, files deleted at once and files whose deletion waits; the dump restored
#    to another database and checked (deploy/drill.sh): every file a row names opens as its row says but the one deleted
#    at once; the live restore marker never goes back, and the live database is not held.
# 5. Direct links: switched on, a download (asked as a browser asks, Accept-Encoding: gzip) answers with a link the
#    store honours, which opens the file and stops working after its life.
# 6. No keys: a server given none, on a machine with a container's identity (served here, as EKS Pod Identity serves
#    it), reads and writes the store, which checks the signatures; without the identity it does not start.
#
# The databases are created and dropped here (their names start with the one given), by the servers' role. Settings:
# FERRUM_BIN (default target/debug/ferrum, built with `cargo build -p ferrum-server --no-default-features --bin ferrum`),
# OBJECT_APP_URL (the servers' role, which may create databases; default postgres://ferrum:ferrum@localhost:5432),
# OBJECT_PYTHON (a Python with moto[server]==5.0.28 and boto3; default python3), OBJECT_PORT (the first of eight ports
# from it, and the store's inner ones from it + 10 and + 10010; default 39200). Every process it starts is stopped on
# the way out.
set -euo pipefail
PREFIX=ferrum_objrehearsal
[ $# -eq 1 ] || { echo "usage: $0 ${PREFIX}_<name>" >&2; exit 2; }
NAME=$1
[[ "$NAME" == "$PREFIX"* && "$NAME" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $NAME does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
APP=${OBJECT_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
PY=${OBJECT_PYTHON:-python3}
BASE=${OBJECT_PORT:-39200}
A_PORT=$BASE B_PORT=$((BASE + 1)) S3_PORT=$((BASE + 2)) MOTO_PORT=$((BASE + 3)) CRED_PORT=$((BASE + 4))
"$PY" -c 'import boto3, moto' 2>/dev/null ||
  { echo "refused: $PY lacks boto3 and moto (python3 -m venv v && v/bin/pip install 'moto[server]==5.0.28'; OBJECT_PYTHON=v/bin/python)" >&2; exit 2; }
command -v weed >/dev/null || { echo "refused: no weed (SeaweedFS) on the PATH (brew install seaweedfs)" >&2; exit 2; }
for port in $(seq "$BASE" $((BASE + 7))) $((BASE + 10)) $((BASE + 11)) $((BASE + 12)) $((BASE + 10010)) $((BASE + 10011)) $((BASE + 10012)); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$port (set OBJECT_PORT)" >&2; exit 2; fi
done
work=$(mktemp -d)
A_PID='' B_PID='' STORE_PID='' MOTO_PID='' CRED_PID=''
DBS=("${NAME}_live" "${NAME}_fresh" "${NAME}_restored" "${NAME}_bare")

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$APP/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }
end() { [ -z "$1" ] || { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
cleanup() {
  status=$?
  for p in "$A_PID" "$B_PID" "$STORE_PID" "$MOTO_PID" "$CRED_PID"; do end "$p"; done
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -25 "$log"; }; done
  fi
  for db in "${DBS[@]}"; do psql "$APP/postgres" -qc "drop database if exists $db with (force)" >/dev/null 2>&1 || true; done
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
for db in "${DBS[@]}"; do
  psql "$APP/postgres" -v ON_ERROR_STOP=1 -qc "drop database if exists $db with (force)" -c "create database $db owner ferrum" >/dev/null
done

export JWT_SECRET FERRUM_SECRET_KEY
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
export LOG_LEVEL=warn FERRUM_BOOTSTRAP_TOKEN=rehearsal-token FERRUM_REQUIRE_OBJECT_STORAGE=false
PASSWORD='Rehearsal-Passw0rd'
mkdir -p "$work/files" "$work/run"
STORE=(FERRUM_S3_BUCKET=ferrum FERRUM_S3_ENDPOINT="http://127.0.0.1:$S3_PORT" FERRUM_S3_REGION=us-east-1 FERRUM_TEST_RUN_DIR="$work/run")

# Runs the binary on a database (production settings unless LOCO_ENV says otherwise), from the server's directory.
on() { local db=$1; shift; (cd "$ROOT/server" && exec env LOCO_ENV="${LOCO_ENV:-production}" DATABASE_URL="$APP/$db" FERRUM_FILES_DIR="$work/files" "$@"); }
# Starts a server on a database and port with more settings, and waits until it answers, or until it ends: 0 when it answers.
serve() {
  local db=$1 port=$2 log=$3; shift 3
  on "$db" PORT="$port" BINDING=127.0.0.1 PUBLIC_URL="http://127.0.0.1:$port" "$@" "$BIN" start >"$work/$log.log" 2>&1 &
  SERVED=$!
  for _ in $(seq 1 120); do
    curl -fsS "http://127.0.0.1:$port/api/ready" >/dev/null 2>&1 && return 0
    kill -0 "$SERVED" 2>/dev/null || { wait "$SERVED" 2>/dev/null || true; SERVED=''; return 1; }
    sleep 1
  done
  fail "the server on $db ($log) neither answered nor ended"
}
token() { curl -fsS "$1/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"files\",\"email\":\"admin@files.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
# Uploads a file of N bytes named NAME; prints its id and SHA-256.
upload() {
  head -c "$3" /dev/urandom >"$work/$4"
  curl -fsS "$1/api/files" -H "authorization: Bearer $2" -F "file=@$work/$4" | json 'd["id"] + " " + d["sha256"]'
}
# The SHA-256 of a file downloaded through a server (with the headers given after).
fetched() {
  local url=$1 tok=$2 id=$3; shift 3
  curl -fsSL "$url/files/$id" -H "authorization: Bearer $tok" "$@" | shasum -a 256 | cut -d' ' -f1
}
doctor_line() { on "$1" "${@:3}" "$BIN" task doctor format:json 2>/dev/null | json "next((f['level'] + ': ' + f['detail']) for f in d if f['check'] == '$2')" || true; }

step "1. a new installation on a local directory is refused"
if serve "${NAME}_fresh" "$A_PORT" fresh; then fail "the server started on a new database with a local directory"; fi
grep -q "a new installation does not keep files in a local directory" "$work/fresh.log" || fail "the refusal does not say why"
grep -q "FERRUM_S3_BUCKET" "$work/fresh.log" || fail "the refusal does not say what to set"

step "2. an installation keeping its files in a directory: made in development, started in production"
LOCO_ENV=development serve "${NAME}_live" "$A_PORT" made || fail "the development server did not start"
A_PID=$SERVED
A="http://127.0.0.1:$A_PORT" B="http://127.0.0.1:$B_PORT"
curl -fsS "$A/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"files\",\"name\":\"Files\",\"admin_email\":\"admin@files.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
T=$(token "$A")
# Enough files, half a megabyte each, for the move to be stopped partway.
: >"$work/old.txt"
for i in $(seq 1 120); do upload "$A" "$T" 524288 "old-$i.bin" >>"$work/old.txt"; done
end "$A_PID"
serve "${NAME}_live" "$A_PORT" kept || fail "an installation keeping its files in a directory did not start"
A_PID=$SERVED
grep -q "files are kept in a local directory" "$work/kept.log" || fail "no warning at start about the directory"
read -r id sha < <(head -1 "$work/old.txt")
[ "$(fetched "$A" "$(token "$A")" "$id")" = "$sha" ] || fail "a file of the directory, through the server that keeps it"
line=$(doctor_line "${NAME}_live" storage.directory)
[[ "$line" == blocker:* ]] || fail "the doctor's line about the directory is not error-level: $line"
end "$A_PID"

step "the bundled store's first start (deploy/store/start.sh, SeaweedFS), its keys handed over"
STORE_DATA="$work/store" STORE_HANDOFF="$work/run/object-store" STORE_BIND=127.0.0.1 STORE_PORT=$S3_PORT \
  STORE_ARGS="-master.port=$((BASE + 10)) -volume.port=$((BASE + 11)) -filer.port=$((BASE + 12)) -master.volumeSizeLimitMB=64" \
  sh "$ROOT/deploy/store/start.sh" >"$work/store.log" 2>&1 &
STORE_PID=$!
for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:$S3_PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://127.0.0.1:$S3_PORT/healthz" >/dev/null || fail "the store did not come up"
[ -f "$work/run/object-store/keys.json" ] || fail "the store's first start handed over no keys"

step "two servers set to the store, files moving: each reads what the other wrote, and the directory's files"
serve "${NAME}_live" "$A_PORT" a "${STORE[@]}" FERRUM_S3_SET_UP_BUCKET=true || fail "server A did not start on the store"
A_PID=$SERVED
grep -q "files are moving from the local directory" "$work/a.log" || fail "no warning that files are moving"
for _ in $(seq 1 30); do [ -f "$work/run/object-store/keys.json" ] || break; sleep 1; done
[ ! -f "$work/run/object-store/keys.json" ] || fail "the handed-over keys' file is still there"
[ "$(sql "${NAME}_live" "set ferrum.vault = 'on'; select count(distinct name) from vault.secret where name in ('aws_access_key_id', 'aws_secret_access_key')" | tail -1)" = 2 ] ||
  fail "the store's keys are not in the vault"
serve "${NAME}_live" "$B_PORT" b "${STORE[@]}" || fail "server B did not start on the store"
B_PID=$SERVED
TA=$(token "$A") TB=$(token "$B")
read -r ida shaa < <(upload "$A" "$TA" 70000 a.bin)
read -r idb shab < <(upload "$B" "$TB" 80000 b.bin)
[ "$(fetched "$B" "$TB" "$ida")" = "$shaa" ] || fail "B does not read what A wrote"
[ "$(fetched "$A" "$TA" "$idb")" = "$shab" ] || fail "A does not read what B wrote"
[ "$(fetched "$B" "$TB" "$id")" = "$sha" ] || fail "B does not read the directory's file while files move"
[ -f "$work/files/tenants/$(sql "${NAME}_live" "set ferrum.system = 'on'; select tenant_id from file_object limit 1" | tail -1)/$ida" ] &&
  fail "a file written while files move went to the directory"

step "files_move, killed partway, then run again"
on "${NAME}_live" "${STORE[@]}" "$BIN" task files_move >"$work/move1.log" 2>&1 &
MOVE=$!
copied=0
for _ in $(seq 1 400); do
  copied=$(sql "${NAME}_live" "set ferrum.system = 'on'; select count(*) from file_move" | tail -1)
  [ "$copied" -ge 8 ] && break
  kill -0 "$MOVE" 2>/dev/null || break
  sleep 0.05
done
kill -STOP "$MOVE" 2>/dev/null || fail "the move ended before it could be stopped ($copied copied): give it more files"
kill -KILL "$MOVE"
wait "$MOVE" 2>/dev/null || true
copied=$(sql "${NAME}_live" "set ferrum.system = 'on'; select count(*) from file_move" | tail -1)
[ "$(sql "${NAME}_live" "set ferrum.system = 'on'; select state from file_store" | tail -1)" = moving ] || fail "a killed move confirmed"
echo "    killed after $copied files"
on "${NAME}_live" "${STORE[@]}" "$BIN" task files_move >"$work/move2.log" 2>&1 || fail "the second run of files_move"
grep -q "is no longer read" "$work/move2.log" || fail "the move did not confirm: $(tail -3 "$work/move2.log")"
grep -q "copied before or already in the store" "$work/move2.log" || fail "the second run said nothing of what was copied"
echo "    $(head -1 "$work/move2.log")"
[ "$(sql "${NAME}_live" "set ferrum.system = 'on'; select state from file_store" | tail -1)" = store ] || fail "the move is not recorded"

step "the directory moved away: both servers, restarted, serve every file as its row says"
end "$A_PID"; end "$B_PID"
mv "$work/files" "$work/files.moved"
mkdir -p "$work/files"
serve "${NAME}_live" "$A_PORT" a2 "${STORE[@]}" || fail "A did not start again"
A_PID=$SERVED
serve "${NAME}_live" "$B_PORT" b2 "${STORE[@]}" || fail "B did not start again"
B_PID=$SERVED
TA=$(token "$A") TB=$(token "$B")
while read -r fid fsha; do
  [ "$(fetched "$B" "$TB" "$fid")" = "$fsha" ] || fail "file $fid after the move"
done <"$work/old.txt"
on "${NAME}_live" "${STORE[@]}" "$BIN" task check_files digest:yes >"$work/check.log" 2>&1 || fail "check_files after the move: $(tail -3 "$work/check.log")"
echo "    $(tail -1 "$work/check.log")"

step "3. the doctor's lines on moto: bad, then good as versioning, lifecycle and encryption are switched on"
"$PY" -m moto.server -H 127.0.0.1 -p "$MOTO_PORT" >"$work/moto.log" 2>&1 &
MOTO_PID=$!
for _ in $(seq 1 30); do curl -fsS "http://127.0.0.1:$MOTO_PORT/moto-api/" >/dev/null 2>&1 && break; sleep 1; done
s3() { "$PY" - "$@" <<'PY'
import sys, boto3
from botocore.config import Config
endpoint, op = sys.argv[1], sys.argv[2]
s3 = boto3.client("s3", endpoint_url=endpoint, aws_access_key_id="moto", aws_secret_access_key="moto", region_name="us-east-1", config=Config(s3={"addressing_style": "path"}))
if op == "make":
    s3.create_bucket(Bucket="bare")
elif op == "versioning":
    s3.put_bucket_versioning(Bucket="bare", VersioningConfiguration={"Status": "Enabled"})
elif op == "lifecycle":
    s3.put_bucket_lifecycle_configuration(Bucket="bare", LifecycleConfiguration={"Rules": [{"ID": "old", "Status": "Enabled", "Filter": {"Prefix": ""}, "NoncurrentVersionExpiration": {"NoncurrentDays": 30}}]})
elif op == "encryption":
    s3.put_bucket_encryption(Bucket="bare", ServerSideEncryptionConfiguration={"Rules": [{"ApplyServerSideEncryptionByDefault": {"SSEAlgorithm": "AES256"}}]})
PY
}
MOTO=(FERRUM_S3_BUCKET=bare FERRUM_S3_ENDPOINT="http://127.0.0.1:$MOTO_PORT" FERRUM_S3_REGION=us-east-1 AWS_ACCESS_KEY_ID=moto AWS_SECRET_ACCESS_KEY=moto FERRUM_BACKUP_REACH_DAYS=30)
s3 "http://127.0.0.1:$MOTO_PORT" make
for check in versioning lifecycle encryption; do
  line=$(doctor_line "${NAME}_bare" "storage.$check" "${MOTO[@]}")
  [[ "$line" == warning:* ]] || fail "storage.$check before it is switched on: $line"
  echo "    before: storage.$check $line"
  s3 "http://127.0.0.1:$MOTO_PORT" "$check"
  line=$(doctor_line "${NAME}_bare" "storage.$check" "${MOTO[@]}")
  [[ "$line" == ok:* ]] || fail "storage.$check once switched on: $line"
  echo "    after:  storage.$check $line"
done
line=$(doctor_line "${NAME}_bare" storage.public "${MOTO[@]}")
[[ "$line" == blocker:* ]] || fail "moto takes requests without keys, and the doctor does not say so: $line"
echo "    moto: storage.public $line"
for check in versioning lifecycle encryption public clock; do
  line=$(doctor_line "${NAME}_live" "storage.$check" "${STORE[@]}")
  [[ "$line" == ok:* ]] || fail "the bundled store, set up at the first start: storage.$check $line"
done
echo "    the bundled store: versioning, lifecycle, encryption, not public, the clock: ok"

step "4. the drill: a database restored to a moment T, every file its rows name opened"
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_delete_after_days value:30 >/dev/null
marker() { sql "${NAME}_live" "set ferrum.system = 'on'; select coalesce(restore_marker, -1) from platform_incarnation" | tail -1; }
before=$(marker)
"$ROOT/deploy/dump.sh" dump "$APP/${NAME}_live" "$work/t.dump" >/dev/null
read -r gone_now _ < <(sed -n 2p "$work/old.txt")
read -r gone_later _ < <(sed -n 3p "$work/old.txt")
sleep 2 # the live servers take the setting within seconds
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_delete_after_days value:0 >/dev/null
sleep 2
curl -fsS -X DELETE "$A/api/files/$gone_now" -H "authorization: Bearer $TA" >/dev/null || fail "deleting a file at once"
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_delete_after_days value:30 >/dev/null
sleep 2
curl -fsS -X DELETE "$A/api/files/$gone_later" -H "authorization: Bearer $TA" >/dev/null || fail "deleting a file whose deletion waits"
read -r _ _ < <(upload "$A" "$TA" 1000 after.bin)
[ "$(sql "${NAME}_live" "set ferrum.system = 'on'; select count(*) from object_release" | tail -1)" = 1 ] || fail "the waiting deletion is not recorded"
(cd "$ROOT/server" && env LOCO_ENV=production FERRUM_FILES_DIR="$work/files" "${STORE[@]}" FERRUM_BIN="$BIN" \
  "$ROOT/deploy/drill.sh" "$work/t.dump" "$APP/${NAME}_restored") >"$work/drill.log" 2>&1 && fail "the drill found the file deleted at once"
grep -q "1 missing" "$work/drill.log" || fail "the drill: $(tail -4 "$work/drill.log")"
grep -q "0 not as their rows say" "$work/drill.log" || fail "the drill: $(tail -4 "$work/drill.log")"
grep -q "$gone_now" "$work/drill.log" || fail "the drill does not name the file deleted at once"
grep -q "$gone_later" "$work/drill.log" && fail "the drill misses the file whose deletion waits"
echo "    $(grep -E 'checked|read through' "$work/drill.log" | tr '\n' ' ')"
sleep 65 # a look at the marker by each live server
after=$(marker)
[ "$after" -ge "$before" ] || fail "the live restore marker went back: $before, then $after"
[ "$(sql "${NAME}_live" "set ferrum.system = 'on'; select held_since is null from platform_incarnation" | tail -1)" = t ] || fail "the live database is held after the drill"
echo "    the live restore marker: $before, then $after; the live database is not held"

step "5. direct links: a download answers with a link the store honours"
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_direct_links value:true >/dev/null
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_direct_link_seconds value:3 >/dev/null
sleep 3
answer=$(curl -sS -o "$work/redirect.body" -w '%{http_code} %{redirect_url}' "$A/files/$ida" -H "authorization: Bearer $TA" -H 'Accept-Encoding: gzip')
read -r code link <<<"$answer"
[ "$code" = 307 ] || fail "a download with direct links on answered $code"
[[ "$link" == "http://127.0.0.1:$S3_PORT/"* ]] || fail "the link is not the store's: $link"
[ "$(curl -fsS "$link" | shasum -a 256 | cut -d' ' -f1)" = "$shaa" ] || fail "the link does not open the file"
sleep 5
[ "$(curl -sS -o /dev/null -w '%{http_code}' "$link")" = 403 ] || fail "the link still works after its life"
on "${NAME}_live" "${STORE[@]}" "$BIN" task set key:files_direct_links value:false >/dev/null
sleep 3
[ "$(curl -sS -o /dev/null -w '%{http_code}' "$A/files/$ida" -H "authorization: Bearer $TA" -H 'Accept-Encoding: gzip')" = 200 ] ||
  fail "a download with direct links off is not through the server"

step "6. no keys: a machine's identity, from a container credentials address served here"
end "$A_PID"; end "$B_PID"
A_PID='' B_PID=''
keys=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["identities"][0]["credentials"][0]; print(c["accessKey"], c["secretKey"])' "$work/store/ferrum/s3.json")
python3 - "$CRED_PORT" "$keys" "$work/creds.jsonl" >"$work/creds.log" 2>&1 <<'PY' &
import http.server, json, sys
port, (key, secret), out = int(sys.argv[1]), sys.argv[2].split(), sys.argv[3]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(out, "a") as f:
            f.write(self.path + "\n")
        body = json.dumps({"AccessKeyId": key, "SecretAccessKey": secret, "Token": "rehearsal-session", "Expiration": "2099-01-01T00:00:00Z"}).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
CRED_PID=$!
sleep 1
NOKEYS=(FERRUM_S3_BUCKET=ferrum FERRUM_S3_ENDPOINT="http://127.0.0.1:$S3_PORT" FERRUM_S3_REGION=us-east-1 FERRUM_TEST_RUN_DIR="$work/none"
  AWS_SHARED_CREDENTIALS_FILE=/nonexistent AWS_CONFIG_FILE=/nonexistent AWS_EC2_METADATA_DISABLED=true)
if serve "${NAME}_fresh" "$A_PORT" nokeys "${NOKEYS[@]}"; then fail "a server with no keys and no identity started"; fi
grep -q "has no keys" "$work/nokeys.log" || fail "the refusal without keys does not say why"
serve "${NAME}_fresh" "$A_PORT" identity "${NOKEYS[@]}" AWS_CONTAINER_CREDENTIALS_FULL_URI="http://127.0.0.1:$CRED_PORT/creds" ||
  fail "a server with a machine's identity did not start"
A_PID=$SERVED
curl -fsS "$A/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"files\",\"name\":\"Files\",\"admin_email\":\"admin@files.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
TI=$(token "$A")
read -r idi shai < <(upload "$A" "$TI" 4096 identity.bin)
[ "$(fetched "$A" "$TI" "$idi")" = "$shai" ] || fail "a file through a server with a machine's identity"
[ -s "$work/creds.jsonl" ] || fail "the credentials address was never asked"
line=$(doctor_line "${NAME}_fresh" storage.versioning "${NOKEYS[@]}" AWS_CONTAINER_CREDENTIALS_FULL_URI="http://127.0.0.1:$CRED_PORT/creds")
[[ "$line" == ok:* ]] || fail "the doctor with a machine's identity: storage.versioning $line"
echo "    written and read with keys from the identity ($(wc -l <"$work/creds.jsonl" | tr -d ' ') asks); the doctor asks the bucket the same way"

step "PASSED"
