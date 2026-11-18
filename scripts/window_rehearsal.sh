#!/usr/bin/env bash
# Rehearses the two-version window (docs/operations.md, "Rolling back and the two-version window") with two real
# binaries on one throwaway database, PostgreSQL and the binaries alone (no Docker): an older build of main and this one.
#
#   scripts/window_rehearsal.sh <older commit> ferrum_windowrehearsal_<name>
#
# The older build: the tree of <older commit> (a commit of main that has the window and the kernel's one baseline
# migration, before the newest change to it), read
# with `git archive` into WINDOW_WORK and built there in a directory of its own, offline, with the core packages only and
# BUILD_DATE set to the commit's date as the image build does. Its build directory is deleted afterwards and the binary
# kept, so the next run reuses it. The build takes about 3 GB while it runs: it is refused with less than 8 GB free.
#
# Then, on a fresh database:
# 1. the older build starts and makes a workspace with a record; this build starts and migrates; it stores telemetry
#    settings with their collector's secret, the chart's key file is mounted (its next start adds the vault's file
#    slot), and an app's setting is stored as <app>.<key>; with its background work off it leaves an event and a job of
#    its own waiting;
# 2. the older build starts again, with FERRUM_MIGRATE_AT_START=false and then true: each time it must start as a guest,
#    read and write the records it knows, leave the schema, the vault and the stored settings exactly as they were, and
#    leave this build's waiting work alone (this build then takes it);
# 3. with SeaweedFS on the PATH (deploy/store/start.sh, as Compose runs it): this build moves the files to object
#    storage (`files_move`) and the directory goes; the older build starts on that database and reads and writes files
#    there, and this build reads what it wrote. Every start after this one is given the store's settings.
#
# The database is created and dropped here, so its name must start with ferrum_windowrehearsal_. Settings: FERRUM_BIN
# (this build; default target/debug/ferrum, built with `cargo build -p ferrum-server --no-default-features --bin ferrum`),
# WINDOW_WORK (where the older build is made and kept; default ${TMPDIR:-/tmp}/ferrum-window), WINDOW_ADMIN_URL (a
# superuser, to create the database; default postgres://$USER@localhost:5432), WINDOW_COMPANION_BIN (this build's server with
# the stand-in for the Cloud app, server/examples/oidc_rehearsal.rs; default target/debug/examples/oidc_rehearsal; without
# it the step with a program beside the workspace is skipped), WINDOW_APP_URL (the servers' role;
# default postgres://ferrum:ferrum@localhost:5432), WINDOW_PORT (default 5395), WINDOW_STORE_PORT (the store's S3 port,
# and its inner ones from it + 10 and + 10010; default 39400).
set -euo pipefail
PREFIX=ferrum_windowrehearsal_
[ $# -eq 2 ] || { echo "usage: $0 <older commit> ${PREFIX}<name>" >&2; exit 2; }
OLD_COMMIT=$1 DB=$2
[[ "$DB" == "$PREFIX"* && "$DB" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $DB does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
WORK=${WINDOW_WORK:-${TMPDIR:-/tmp}/ferrum-window}
ADMIN=${WINDOW_ADMIN_URL:-postgres://$USER@localhost:5432}
APP=${WINDOW_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
PORT=${WINDOW_PORT:-5395}
URL="http://127.0.0.1:$PORT"

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$ADMIN/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }

# The older build, made once.
COMMIT=$(git -C "$ROOT" rev-parse --verify "$OLD_COMMIT^{commit}") || { echo "refused: $OLD_COMMIT is not a commit here" >&2; exit 2; }
SHORT=${COMMIT:0:8}
OLD_DIR=$WORK/$SHORT
OLD_BIN=$OLD_DIR/ferrum
OLD_DATE=$(git -C "$ROOT" log -1 --format=%ct "$COMMIT")
if [ ! -x "$OLD_BIN" ]; then
  mkdir -p "$WORK"
  free_kb=$(df -k "$WORK" | awk 'NR == 2 { print $4 }')
  [ "$free_kb" -ge $((8 * 1024 * 1024)) ] || { echo "refused: building $SHORT needs about 3 GB, and less than 8 GB is free" >&2; exit 2; }
  step "building the older build $SHORT (its commit's date: $(python3 -c 'import datetime, sys; print(datetime.datetime.fromtimestamp(int(sys.argv[1]), datetime.timezone.utc).isoformat())' "$OLD_DATE")) in $OLD_DIR"
  rm -rf "$OLD_DIR/src" "$OLD_DIR/target"
  mkdir -p "$OLD_DIR/src"
  git -C "$ROOT" archive "$COMMIT" | tar -x -C "$OLD_DIR/src"
  (cd "$OLD_DIR/src" && CARGO_TARGET_DIR="$OLD_DIR/target" CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 BUILD_DATE=$OLD_DATE \
    cargo build --offline --locked -p ferrum-server --no-default-features --bin ferrum)
  cp "$OLD_DIR/target/debug/ferrum" "$OLD_BIN"
  rm -rf "$OLD_DIR/target"
fi
# Each build runs in its own tree's server directory: its own settings files, as its image carries them.
[ -d "$OLD_DIR/src/server/config" ] || { echo "refused: $OLD_DIR/src/server/config is missing; delete $OLD_DIR to build it again" >&2; exit 2; }

# Another server on the port would pass for one of these.
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$PORT (set WINDOW_PORT)" >&2; exit 2; fi
work=$(mktemp -d)
SERVER_PID='' STORE_PID=''
STORE_ENV=()
cleanup() {
  status=$?
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null
  [ -n "$STORE_PID" ] && kill "$STORE_PID" 2>/dev/null && wait "$STORE_PID" 2>/dev/null
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -30 "$log"; }; done
  fi
  sql postgres "drop database if exists $DB with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

export JWT_SECRET
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
# Production settings, the only ones that read FERRUM_MIGRATE_AT_START; a files directory in place of a bucket.
export LOCO_ENV=production LOG_LEVEL=warn FERRUM_BOOTSTRAP_TOKEN=rehearsal-token FERRUM_REQUIRE_OBJECT_STORAGE=false
PASSWORD='Rehearsal-Passw0rd'
mkdir -p "$work/files" "$work/run"
# The files of /run/ferrum, where a debug build reads them (the chart's key file, the store's handoff).
export FERRUM_TEST_RUN_DIR="$work/run"

# Starts a build (old or new) and waits until it answers, or until it ends (a refusal): 0 when it answers.
start() {
  local which=$1 log=$2; shift 2
  local bin=$BIN dir=$ROOT/server
  [ "$which" = old ] && bin=$OLD_BIN dir=$OLD_DIR/src/server
  [ "$which" = companion ] && bin=$COMPANION_BIN
  (cd "$dir" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" PORT="$PORT" BINDING=127.0.0.1 PUBLIC_URL="$URL" \
    ${STORE_ENV[@]+"${STORE_ENV[@]}"} "$@" "$bin" start) >"$work/$log.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 180); do
    curl -fsS "$URL/api/ready" >/dev/null 2>&1 && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || { wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=''; return 1; }
    sleep 1
  done
  fail "the $which build neither answered nor ended ($log)"
}
stop() { [ -n "$SERVER_PID" ] && { kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; }; SERVER_PID=''; }
token() { curl -fsS "$URL/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"window\",\"email\":\"admin@window.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]'; }
contact() { curl -sS -o /dev/null -w '%{http_code}' "$URL/api/v1/Contact" -H "authorization: Bearer $1" -H 'content-type: application/json' -d "{\"first_name\":\"$2\"}"; }
names() { curl -fsS "$URL/api/v1/Contact?limit=100" -H "authorization: Bearer $1" | json '" ".join(sorted(r["first_name"] for r in d["rows"]))'; }
# The schema as a digest: columns, indexes, constraints, functions, migrations and what the window records.
schema() {
  sql "$DB" "select md5(string_agg(x, '|' order by x)) from (
    select table_name || '.' || column_name || ':' || data_type || ':' || is_nullable || ':' || coalesce(column_default, '') from information_schema.columns where table_schema = 'public'
    union all select indexname || ':' || indexdef from pg_indexes where schemaname = 'public'
    union all select conname || ':' || pg_get_constraintdef(oid) from pg_constraint where connamespace = 'public'::regnamespace
    union all select p.oid::regprocedure::text || ':' || md5(p.prosrc) from pg_proc p where p.pronamespace = 'public'::regnamespace
    union all select version::text || ':' || checksum::text from _sqlx_migrations
    union all select key || '=' || value from cluster_state where key like 'schema_%') s(x)"
}
# The vault and the stored settings as a digest, which the older build knows nothing of.
kept() {
  sql "$DB" "select md5(string_agg(x, '|' order by x)) from (
    select kind || ':' || md5(wrapped) from vault.root_slot
    union all select scope::text || '/' || owner || '/' || name || ':' || version || ':' || md5(sealed) from vault.secret
    union all select scope || '/' || scope_name || '/' || key || '=' || coalesce(value::text, '') || ':' || version from setting) s(x)"
}
# Work this build left waiting, which the older build must not take: events not handled and jobs not started.
waiting() {
  sql "$DB" "select (select count(*) from outbox where processed_at is null and not dead and build_date > to_timestamp($OLD_DATE)) || ' ' ||
    (select count(*) from job where status = 'queued' and build_date > to_timestamp($OLD_DATE))"
}
guest() {
  local mode=$1
  start old "old-$mode" FERRUM_MIGRATE_AT_START="$mode" || fail "the older build did not start on the newer database (migrate at start: $mode)"
  grep -q "as a guest" "$work/old-$mode.log" || fail "the older build did not say it runs as a guest (migrate at start: $mode)"
  local t
  t=$(token)
  [ "$(contact "$t" "Guest-$mode")" = 201 ] || fail "the guest could not write"
  local seen
  seen=$(names "$t")
  for n in Old-1 New-1 Queued "Guest-$mode"; do [[ " $seen " == *" $n "* ]] || fail "the guest does not read $n ($seen)"; done
  sleep 8 # its background work runs meanwhile
  [ "$(waiting)" = "$WAITING" ] || fail "the guest took the newer build's waiting work: $(waiting), was $WAITING"
  curl -fsS "$URL/metrics" | grep -q '^ferrum_outbox_waiting_newer_build [1-9]' || fail "the guest does not count the newer build's waiting event"
  stop
  [ "$(schema)" = "$SCHEMA" ] || fail "the guest changed the schema (migrate at start: $mode)"
  [ "$(kept)" = "$KEPT" ] || fail "the guest changed the vault or the stored settings (migrate at start: $mode)"
  echo "    a guest (migrate at start: $mode): read and wrote, left the schema, the vault, the stored settings and the newer build's event and job alone"
}

step "the older build ($SHORT) on a fresh database: a workspace, a record and a file"
sql postgres "drop database if exists $DB with (force)"
sql postgres "create database $DB owner ferrum"
start old old-first || fail "the older build did not start on an empty database"
curl -fsS "$URL/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"window\",\"name\":\"Window\",\"admin_email\":\"admin@window.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
T=$(token)
[ "$(contact "$T" Old-1)" = 201 ] || fail "a record with the older build"
# An installation keeping its files in a local directory, as one from before object storage only (D6) does.
FIRST=$(curl -fsS "$URL/api/files" -H "authorization: Bearer $T" -F "file=@$ROOT/README.md" | json 'd["id"] + " " + d["sha256"]') ||
  fail "a file with the older build"
stop

step "this build migrates; with its background work off it leaves an event and a job waiting"
start new new-first || fail "this build did not start"
grep -q "files are kept in a local directory" "$work/new-first.log" || fail "this build did not warn about the files' directory"
T=$(token)
[ "$(contact "$T" New-1)" = 201 ] || fail "a record with this build"
stop
# Telemetry settings stored with their collector's secret (a port nothing listens on), the chart's key Secret mounted
# (deploy/helm/ferrum/templates/vault-key.yaml), and an app's setting as <app>.<key>: this build carries no app, so the
# row is written as a build with Drive declaring office_url (crates/apps, operator_settings) stores it.
task() { (cd "$ROOT/server" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" "$BIN" task "$@") >>"$work/tasks.log" 2>&1 || fail "ferrum task $1: $(tail -3 "$work/tasks.log")"; }
printf 'authorization=Bearer%%20rehearsal' | task set key:otel_exporter_otlp_endpoint value:"http://127.0.0.1:$((PORT + 1))" secret:stdin
task set key:otel_service_name value:ferrum-window
openssl rand -base64 32 | tr -d '\n' >"$work/run/root.key"
sql "$DB" "insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', 'drive.office_url', '\"https://office.window.test\"', 'window rehearsal')"
start new new-quiet FERRUM_RUN_BACKGROUND=false || fail "this build did not start without background work"
[ "$(sql "$DB" "select string_agg(kind, ' ' order by kind) from vault.root_slot")" = "credential file" ] ||
  fail "this build's start did not add the key file's slot: $(sql "$DB" "select string_agg(kind, ' ' order by kind) from vault.root_slot")"
STORED=$(sql "$DB" "select string_agg(key, ' ' order by key) from setting where value is not null and (key like 'otel%' or key like '%.%')")
[ "$STORED" = "drive.office_url otel_exporter_otlp_endpoint otel_service_name" ] || fail "stored: $STORED"
[ "$(sql "$DB" "select count(*) from vault.secret where name = 'otel_exporter_otlp_headers'")" = 1 ] || fail "the collector's secret is not in the vault"
echo "    the vault's slots: credential file; stored: $STORED, and the collector's secret in the vault"
T=$(token)
[ "$(contact "$T" Queued)" = 201 ] || fail "the waiting record"
curl -fsS -o /dev/null "$URL/api/admin/export" -X POST -H "authorization: Bearer $T" || fail "the waiting export"
stop
WAITING=$(waiting)
read -r events jobs <<<"$WAITING"
[ "$events" -ge 1 ] && [ "$jobs" -ge 1 ] || fail "this build left no event or no job waiting: $WAITING"
echo "    waiting for this build: $events events, $jobs jobs"
SCHEMA=$(schema)
KEPT=$(kept)

step "the older build again, as a guest"
guest false
guest true

step "this build takes its waiting work"
# Background work on again by name: the quiet start's FERRUM_RUN_BACKGROUND=false was copied into the settings table
# (R1, docs/operations.md "Settings in the database"), and only the environment wins over it.
start new new-again FERRUM_RUN_BACKGROUND=true || fail "this build did not start again"
for _ in $(seq 1 30); do [ "$(waiting)" = "0 0" ] && break; sleep 1; done
[ "$(waiting)" = "0 0" ] || fail "this build did not take its own waiting work: $(waiting)"
# The SHA-256 of file $1 downloaded through the server that runs now.
fetched() { curl -fsSL "$URL/files/$1" -H "authorization: Bearer $(token)" | shasum -a 256 | cut -d' ' -f1; }
if command -v weed >/dev/null 2>&1; then
  step "this build moves the files to object storage; the older build reads and writes them there"
  stop
  SPORT=${WINDOW_STORE_PORT:-39400}
  for p in "$SPORT" $((SPORT + 10)) $((SPORT + 11)) $((SPORT + 12)) $((SPORT + 10010)) $((SPORT + 10011)) $((SPORT + 10012)); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then fail "something listens on 127.0.0.1:$p (set WINDOW_STORE_PORT)"; fi
  done
  STORE_DATA="$work/store" STORE_HANDOFF="$work/run/object-store" STORE_BIND=127.0.0.1 STORE_PORT=$SPORT \
    STORE_ARGS="-master.port=$((SPORT + 10)) -volume.port=$((SPORT + 11)) -filer.port=$((SPORT + 12)) -master.volumeSizeLimitMB=64" \
    sh "$ROOT/deploy/store/start.sh" >"$work/store.log" 2>&1 &
  STORE_PID=$!
  for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:$SPORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
  read -r KEY SECRET < <(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["identities"][0]["credentials"][0]; print(c["accessKey"], c["secretKey"])' "$work/store/ferrum/s3.json")
  # The older build takes the store's keys from the environment (it has no vault).
  STORE_ENV=(FERRUM_S3_BUCKET=ferrum "FERRUM_S3_ENDPOINT=http://127.0.0.1:$SPORT" FERRUM_S3_REGION=us-east-1 "FERRUM_TEST_RUN_DIR=$work/run"
    "AWS_ACCESS_KEY_ID=$KEY" "AWS_SECRET_ACCESS_KEY=$SECRET")
  start new new-store || fail "this build did not start with the store"
  grep -q "files are moving from the local directory" "$work/new-store.log" || fail "this build did not say files are moving"
  stop
  (cd "$ROOT/server" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" "${STORE_ENV[@]}" "$BIN" task files_move) >"$work/move.log" 2>&1 ||
    fail "files_move: $(tail -3 "$work/move.log")"
  grep -q "is no longer read" "$work/move.log" || fail "the move was not confirmed: $(tail -3 "$work/move.log")"
  mv "$work/files" "$work/files.moved" && mkdir -p "$work/files"
  start old old-store || fail "the older build did not start on the database this build moved files in"
  read -r first_id first_sha <<<"$FIRST"
  [ "$(fetched "$first_id")" = "$first_sha" ] || fail "the older build does not read its file from object storage"
  OLDER=$(curl -fsS "$URL/api/files" -H "authorization: Bearer $(token)" -F "file=@$ROOT/LICENSE" | json 'd["id"] + " " + d["sha256"]') ||
    fail "the older build does not write a file to object storage"
  read -r older_id older_sha <<<"$OLDER"
  [ "$(fetched "$older_id")" = "$older_sha" ] || fail "the older build does not read back the file it wrote"
  stop
  start new new-after-store || fail "this build did not start again"
  [ "$(fetched "$older_id")" = "$older_sha" ] || fail "this build does not read the file the older build wrote"
  echo "    moved ($(head -1 "$work/move.log")); the older build read its file there, wrote one, and this build reads it"
else
  echo "    skipped moving files to object storage: no SeaweedFS (weed) on the PATH"
fi
# A program an installed app declared beside the workspace (docs/api.md, "Signing in to programs beside Ferrum"), by the
# stand-in for the Cloud app (server/examples/oidc_rehearsal.rs, WINDOW_COMPANION_BIN), and someone signed in through it.
# The older build is asked for a sandbox of the workspace and refuses, as it does for any workspace on a database this
# build migrated (a sandbox needs every replica on the newest build), and leaves nothing. This build's own sandbox has
# none of the program's rows or secrets but the people's subjects. What a copier that takes every table with a
# `tenant_id` would bring into a sandbox (these tables' keys are unique within a workspace, so such a copy meets none of
# the live workspace's) this build, running, takes away within 15 seconds and tells nobody: they are the live
# workspace's sign-ins, which go on. The live workspace still signs people in through the program; the sandbox goes.
COMPANION_BIN=$(cd "$ROOT" && realpath "${WINDOW_COMPANION_BIN:-${CARGO_TARGET_DIR:-target}/debug/examples/oidc_rehearsal}" 2>/dev/null || true)
if [ -n "$COMPANION_BIN" ] && [ -x "$COMPANION_BIN" ]; then
  step "a program declared beside the workspace, someone signed in through it; a sandbox asked of the older build, then made by this one"
  stop
  back="https://forge.window.test/user/oauth2/window/callback"
  (cd "$ROOT/server" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" ${STORE_ENV[@]+"${STORE_ENV[@]}"} "$COMPANION_BIN" setup window "$back") \
    >"$work/companion.json" 2>"$work/companion.log" || fail "declaring the program: $(tail -3 "$work/companion.log")"
  CLIENT=$(json 'd["client_id"]' <"$work/companion.json")
  CLIENT_SECRET=$(json 'd["secret"]' <"$work/companion.json")
  # Someone signs in to the program: the workspace is the server's one here, so 127.0.0.1 is its address (its issuer).
  signs_in() {
    local jar="$work/browser.jar" verifier challenge asked code
    rm -f "$jar"
    curl -fsS -o /dev/null -c "$jar" -b "$jar" -H 'content-type: application/json' -H 'sec-fetch-site: same-origin' \
      -d "{\"tenant\":\"window\",\"email\":\"admin@window.test\",\"password\":\"$PASSWORD\",\"session\":\"cookie\"}" "$URL/api/auth/login" || return 1
    verifier=$(openssl rand -hex 32)
    challenge=$(printf '%s' "$verifier" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
    asked=$(curl -sS -o /dev/null -b "$jar" -w '%{redirect_url}' "$URL/oauth/authorize?response_type=code&client_id=$CLIENT&redirect_uri=$(python3 -c 'import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=""))' "$back")&scope=openid&state=s&code_challenge=$challenge&code_challenge_method=S256")
    code=$(python3 -c 'import urllib.parse,sys; print(urllib.parse.parse_qs(urllib.parse.urlsplit(sys.argv[1]).query)["code"][0])' "$asked") || return 1
    curl -fsS -u "$CLIENT:$CLIENT_SECRET" --data-urlencode grant_type=authorization_code --data-urlencode "code=$code" --data-urlencode "redirect_uri=$back" \
      --data-urlencode "code_verifier=$verifier" "$URL/oauth/token" | json 'd["id_token"].count(".")'
  }
  start companion new-companion FERRUM_DEFAULT_WORKSPACE=window || fail "this build did not start with the program declared"
  [ "$(signs_in)" = 2 ] || fail "nobody signed in through the program before the sandbox"
  stop
  WINDOW_ID=$(sql "$DB" "select id from tenant where slug = 'window'")
  rows() { sql "$DB" "select (select count(*) from oauth_companion where tenant_id = '$1') || ' ' || (select count(*) from oidc_code where tenant_id = '$1') \
    || ' ' || (select count(*) from oidc_sign_in where tenant_id = '$1') || ' ' || (select count(*) from oidc_subject where tenant_id = '$1')"; }
  [ "$(rows "$WINDOW_ID")" = "1 1 1 1" ] || fail "the program's rows before the sandbox: $(rows "$WINDOW_ID")"
  till=$(python3 -c 'import datetime; print(datetime.date.today() + datetime.timedelta(days=10))')
  here() { (cd "$ROOT/server" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" ${STORE_ENV[@]+"${STORE_ENV[@]}"} "$BIN" task "$@"); }
  if (cd "$OLD_DIR/src/server" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" ${STORE_ENV[@]+"${STORE_ENV[@]}"} "$OLD_BIN" task copy_workspace \
    tenant:window as:window-try until:"$till" from:here) >"$work/old-sandbox.log" 2>&1; then
    fail "the older build made a sandbox on a database this build migrated"
  fi
  grep -q "a sandbox needs every replica on this build" "$work/old-sandbox.log" || fail "the older build's sandbox: $(tail -5 "$work/old-sandbox.log")"
  [ -z "$(sql "$DB" "select id from tenant where slug = 'window-try'")" ] && [ "$(rows "$WINDOW_ID")" = "1 1 1 1" ] || fail "the older build left something of a sandbox"
  here copy_workspace tenant:window as:window-try until:"$till" from:here >"$work/sandbox.log" 2>&1 || fail "this build's sandbox: $(tail -5 "$work/sandbox.log")"
  SANDBOX_ID=$(sql "$DB" "select id from tenant where slug = 'window-try' and sandbox_of is not null")
  [ -n "$SANDBOX_ID" ] && [ "$(rows "$SANDBOX_ID")" = "0 0 0 1" ] || fail "this build's sandbox: $(rows "${SANDBOX_ID:-none}") (client, codes, sign-ins, subjects)"
  [ "$(sql "$DB" "select count(*) from vault.secret where scope = '$SANDBOX_ID' and name like 'oidc%'")" = 0 ] || fail "the sandbox has the program's secrets"
  # What a copier of every table with a `tenant_id` would bring, under the sandbox's id.
  for t in oauth_companion oidc_code oidc_sign_in; do
    sql "$DB" "insert into $t select (jsonb_populate_record(null::$t, to_jsonb(r) || jsonb_build_object('tenant_id', '$SANDBOX_ID'::uuid))).* from $t r where r.tenant_id = '$WINDOW_ID'" \
      || fail "copying $t as a copier would"
  done
  [ "$(rows "$SANDBOX_ID")" = "1 1 1 1" ] || fail "the copy: $(rows "$SANDBOX_ID")"
  # Its background work on by name: the quiet start's FERRUM_RUN_BACKGROUND=false is in the settings table.
  start companion new-after-sandbox FERRUM_DEFAULT_WORKSPACE=window FERRUM_RUN_BACKGROUND=true || fail "this build did not start with a sandbox"
  for _ in $(seq 60); do [ "$(rows "$SANDBOX_ID")" = "0 0 0 1" ] && break; sleep 0.5; done
  [ "$(rows "$SANDBOX_ID")" = "0 0 0 1" ] || fail "this build left the program's rows in the sandbox: $(rows "$SANDBOX_ID")"
  [ "$(sql "$DB" "select count(*) from outbox where topic = 'ferrum.companion.signed_out'")" = 0 ] || fail "someone was told a sign-in ended"
  here openid tenant:window-try >"$work/sandbox-openid.log" 2>&1 || fail "ferrum task openid on the sandbox: $(tail -3 "$work/sandbox-openid.log")"
  grep -q "no program signs this workspace's people in" "$work/sandbox-openid.log" || fail "the sandbox has a program: $(cat "$work/sandbox-openid.log")"
  [ "$(signs_in)" = 2 ] || fail "the live workspace no longer signs people in through the program"
  [ "$(rows "$WINDOW_ID")" = "1 2 2 1" ] || fail "the live workspace's rows: $(rows "$WINDOW_ID") (a second sign-in, from a second session)"
  stop
  here delete_sandbox tenant:window-try >"$work/sandbox-delete.log" 2>&1 || fail "deleting the sandbox: $(tail -3 "$work/sandbox-delete.log")"
  [ "$(rows "$SANDBOX_ID")" = "0 0 0 0" ] || fail "the deleted sandbox's rows: $(rows "$SANDBOX_ID")"
  echo "    the older build refused the sandbox and left nothing; this build's has none of the program's rows or secrets but the subject; what a copier of every workspace table would bring there this build took away, telling nobody; the live workspace still signs people in, and the sandbox is gone"
  start new new-before-protections || fail "this build did not start again"
else
  echo "    skipped a program beside the workspace: no stand-in for the Cloud app at ${WINDOW_COMPANION_BIN:-target/debug/examples/oidc_rehearsal}"
fi
echo "the window rehearsal passed"
