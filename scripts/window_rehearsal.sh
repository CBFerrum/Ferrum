#!/usr/bin/env bash
# Rehearses the two-version window (docs/operations.md, "Rolling back and the two-version window") with two real
# binaries on one throwaway database, PostgreSQL and the binaries alone (no Docker): an older build of main and this one.
#
#   scripts/window_rehearsal.sh <older commit> ferrum_windowrehearsal_<name>
#
# The older build: the tree of <older commit> (a commit of main that has the window, before the newest migration), read
# with `git archive` into WINDOW_WORK and built there in a directory of its own, offline, with the core packages only and
# BUILD_DATE set to the commit's date as the image build does. Its build directory is deleted afterwards and the binary
# kept, so the next run reuses it. The build takes about 3 GB while it runs: it is refused with less than 8 GB free.
#
# Then, on a fresh database:
# 1. the older build starts and makes a workspace with a record; this build starts and migrates; with its background
#    work off it leaves an event and a job of its own waiting;
# 2. the older build starts again, with FERRUM_MIGRATE_AT_START=false and then true: each time it must start as a guest,
#    read and write the records it knows, leave the schema exactly as it was, and leave this build's waiting work alone
#    (this build then takes it);
# 3. this build switches on staff networks, which the older build enforces (it still starts), then recorded reads, which
#    it does not: it refuses to start, naming them; `ferrum task allow_older_builds` lets it start; the next start of
#    this build puts the floor back, and it is refused again.
#
# The database is created and dropped here, so its name must start with ferrum_windowrehearsal_. Settings: FERRUM_BIN
# (this build; default target/debug/ferrum, built with `cargo build -p ferrum-server --no-default-features --bin ferrum`),
# WINDOW_WORK (where the older build is made and kept; default ${TMPDIR:-/tmp}/ferrum-window), WINDOW_ADMIN_URL (a
# superuser, to create the database; default postgres://$USER@localhost:5432), WINDOW_APP_URL (the servers' role;
# default postgres://ferrum:ferrum@localhost:5432), WINDOW_PORT (default 5395).
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
  free_kb=$(df -k "$(dirname "$WORK")" | awk 'NR == 2 { print $4 }')
  [ "$free_kb" -ge $((8 * 1024 * 1024)) ] || { echo "refused: building $SHORT needs about 3 GB, and less than 8 GB is free" >&2; exit 2; }
  step "building the older build $SHORT (its commit's date: $(python3 -c 'import datetime, sys; print(datetime.datetime.fromtimestamp(int(sys.argv[1]), datetime.UTC).isoformat())' "$OLD_DATE")) in $OLD_DIR"
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

# Another server answering on the port would pass for one of these.
if curl -fsS -o /dev/null "$URL/api/ready" 2>/dev/null; then echo "refused: something already answers on $URL (set WINDOW_PORT)" >&2; exit 2; fi
work=$(mktemp -d)
SERVER_PID=''
cleanup() {
  status=$?
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log; do [ -f "$log" ] && { echo "--- $log"; tail -30 "$log"; }; done
  fi
  sql postgres "drop database if exists $DB with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

export JWT_SECRET FERRUM_SECRET_KEY
JWT_SECRET=$(openssl rand -base64 48 | tr -d '\n')
FERRUM_SECRET_KEY=$(openssl rand -base64 32 | tr -d '\n')
# Production settings, the only ones that read FERRUM_MIGRATE_AT_START; a files directory in place of a bucket.
export LOCO_ENV=production LOG_LEVEL=warn FERRUM_BOOTSTRAP_TOKEN=rehearsal-token FERRUM_REQUIRE_OBJECT_STORAGE=false
PASSWORD='Rehearsal-Passw0rd'
mkdir -p "$work/files"

# Starts a build (old or new) and waits until it answers, or until it ends (a refusal): 0 when it answers.
start() {
  local which=$1 log=$2; shift 2
  local bin=$BIN dir=$ROOT/server
  [ "$which" = old ] && bin=$OLD_BIN dir=$OLD_DIR/src/server
  (cd "$dir" && exec env DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" PORT="$PORT" BINDING=127.0.0.1 PUBLIC_URL="$URL" "$@" \
    "$bin" start) >"$work/$log.log" 2>&1 &
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
  echo "    a guest (migrate at start: $mode): read and wrote, left the schema and the newer build's event and job alone"
}

step "the older build ($SHORT) on a fresh database: a workspace and a record"
sql postgres "drop database if exists $DB with (force)"
sql postgres "create database $DB owner ferrum"
start old old-first || fail "the older build did not start on an empty database"
curl -fsS "$URL/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
  -d "{\"slug\":\"window\",\"name\":\"Window\",\"admin_email\":\"admin@window.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null
T=$(token)
[ "$(contact "$T" Old-1)" = 201 ] || fail "a record with the older build"
stop

step "this build migrates; with its background work off it leaves an event and a job waiting"
start new new-first || fail "this build did not start"
T=$(token)
[ "$(contact "$T" New-1)" = 201 ] || fail "a record with this build"
stop
start new new-quiet FERRUM_RUN_BACKGROUND=false || fail "this build did not start without background work"
T=$(token)
[ "$(contact "$T" Queued)" = 201 ] || fail "the waiting record"
curl -fsS -o /dev/null "$URL/api/admin/export" -X POST -H "authorization: Bearer $T" || fail "the waiting export"
stop
WAITING=$(waiting)
read -r events jobs <<<"$WAITING"
[ "$events" -ge 1 ] && [ "$jobs" -ge 1 ] || fail "this build left no event or no job waiting: $WAITING"
echo "    waiting for this build: $events events, $jobs jobs"
SCHEMA=$(schema)

step "the older build again, as a guest"
guest false
guest true

step "this build takes its waiting work"
start new new-again || fail "this build did not start again"
for _ in $(seq 1 30); do [ "$(waiting)" = "0 0" ] && break; sleep 1; done
[ "$(waiting)" = "0 0" ] || fail "this build did not take its own waiting work: $(waiting)"
T=$(token)
settings() { curl -sS -o "$work/settings.json" -w '%{http_code}' -X PUT "$URL/api/admin/settings" -H "authorization: Bearer $T" -H 'content-type: application/json' -d "$1"; }
# A build that knows floors (pull request 235 and later) names the protections it is too old for; one from before them
# says only which build to start, and does not enforce staff networks either when it is older than their floor.
FLOORS=$(ls "$OLD_DIR"/src/crates/kernel/migrations/*_window_floor.sql >/dev/null 2>&1 && echo yes || echo no)
if [ "$FLOORS" = yes ]; then
  step "a protection the older build enforces (staff networks): it still starts"
  [ "$(settings '{"staff_networks": {"networks": ["127.0.0.0/8"], "mode": "report"}}')" = 200 ] || fail "staff networks: $(cat "$work/settings.json")"
  stop
  start old old-networks || fail "the older build was refused for a protection it enforces"
  stop
  start new new-reads || fail "this build did not start"
  T=$(token)
fi

step "a protection the older build does not enforce (recorded reads): it is refused, saying what it needs"
[ "$(settings '{"record_reads": {"Contact": "open"}}')" = 200 ] || fail "recorded reads: $(cat "$work/settings.json")"
stop
refused() {
  if start old "$1"; then fail "the older build started though recorded reads are on"; fi
  if [ "$FLOORS" = yes ]; then
    grep -q "record_reads" "$work/$1.log" && grep -q "allow_older_builds" "$work/$1.log" || fail "the refusal does not name what needs a newer build"
    grep -o 'record_reads ([^;]*' "$work/$1.log" | head -1 | sed 's/^/    refused: /'
  else
    grep -q "Start a build from .* or newer" "$work/$1.log" || fail "the refusal does not say which build to start"
    grep -o 'Start a build from [^ ]* or newer' "$work/$1.log" | head -1 | sed 's/^/    refused: /'
  fi
}
refused old-refused

step "allow_older_builds with this build: the older build starts again, until this build's next start"
(cd "$ROOT/server" && DATABASE_URL="$APP/$DB" FERRUM_FILES_DIR="$work/files" "$BIN" task allow_older_builds reason:"window rehearsal") | tail -3
start old old-allowed || fail "the older build was refused after allow_older_builds"
stop
start new new-floor || fail "this build did not start"
stop
refused old-refused-again
echo "the window rehearsal passed"
