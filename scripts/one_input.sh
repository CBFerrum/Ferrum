#!/usr/bin/env bash
# The boot contract, proven (docs/plan/stateless.md, D1; docs/operations.md, "What a replica still reads from its
# environment"): the real server binary, started with DATABASE_URL in its environment and nothing else, on a fresh
# database, as two replicas, doing real work; then the same database started by the previous build.
#
#   scripts/one_input.sh ferrum_oneinput_<name>
#
# Every process of this build (each task and each replica) runs as `env -i DATABASE_URL=… ferrum -e production …`, the
# environment (production) named on the command line. Beside it in the environment there is only the loader's variable
# that puts a small library in front of libc's getenv (DYLD_INSERT_LIBRARIES on macOS, LD_PRELOAD on Linux), which
# writes each name the process asks for, and whether it was set, to its standard error; never a value. The script then
# fails when any variable but DATABASE_URL was found set, when DATABASE_URL was not read at all (the trace would be
# broken), or when a name was read that neither the settings' registry (read through this release only, so that one set
# there wins) nor scripts/settings.py's baseline of what stays in the environment declares. It sees every read, the
# libraries' among them, so it cannot fall behind the code.
#
# What it shows, in order: migrate; the settings applied from a document; the secrets set by tasks (the object store's
# keys, a bootstrap token and the operator's token, made and shown once); a replica on the port the document gave, and a
# second on the port a setting then gave; a workspace made with the token; a sign-in; a record saved on one replica and
# read on the other; a file kept in object storage and read back; a background job done once though both replicas run
# background work; a setting that applies at once changed, and both replicas taking it within the time promised (30 s;
# a notice usually brings it within a second); a restart after which the session from before still works; the doctor
# saying the environment decides nothing; then the previous build started on the same database, signing in and reading
# the record (scripts/window.py's rule: the build before a change still runs on the database it leaves).
#
# Needs: PostgreSQL (the servers' role and a superuser to make the database), SeaweedFS's `weed` for object storage
# (a private one on 127.0.0.1, with a key pair of its own), a C compiler for the getenv library, curl, python3.
# Settings, the script's own (none reaches a server): FERRUM_BIN (this build; default
# $CARGO_TARGET_DIR/debug/ferrum, built with `cargo build -p ferrum-server --no-default-features --bin ferrum`),
# ONEINPUT_OLD (the previous build's commit; default the merge base with origin/main), WINDOW_WORK (where it is made and
# kept, shared with scripts/window_rehearsal.sh; default ${TMPDIR:-/tmp}/ferrum-window; about 3 GB while it builds),
# ONEINPUT_ADMIN_URL (default postgres://$USER@localhost:5432),
# ONEINPUT_APP_URL (default postgres://ferrum:ferrum@localhost:5432), ONEINPUT_PORTS (the two replicas' and the object
# store's ports, default "5391 5392 5393"), WEED (default the PATH's).
set -euo pipefail
PREFIX=ferrum_oneinput_
[ $# -eq 1 ] || { echo "usage: $0 ${PREFIX}<name>" >&2; exit 2; }
DB=$1
[[ "$DB" == "$PREFIX"* && "$DB" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $DB does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
[ -x "$BIN" ] || { echo "no server binary at $BIN (cargo build -p ferrum-server --no-default-features --bin ferrum)" >&2; exit 2; }
WORK=${WINDOW_WORK:-${TMPDIR:-/tmp}/ferrum-window}
ADMIN=${ONEINPUT_ADMIN_URL:-postgres://$USER@localhost:5432}
APP=${ONEINPUT_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
read -r P1 P2 PS <<<"${ONEINPUT_PORTS:-5391 5392 5393}"
WEED=${WEED:-$(command -v weed || true)}
[ -x "$WEED" ] || { echo "no SeaweedFS (weed) for object storage: brew install seaweedfs, or set WEED" >&2; exit 2; }
URL="$APP/$DB"

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sql() { psql "$ADMIN/$1" -v ON_ERROR_STOP=1 -Atqc "$2"; }

# The previous build, made once (as scripts/window_rehearsal.sh makes its older build, in the same place).
OLD_COMMIT=${ONEINPUT_OLD:-$(git -C "$ROOT" merge-base origin/main HEAD)}
COMMIT=$(git -C "$ROOT" rev-parse --verify "$OLD_COMMIT^{commit}") || { echo "refused: $OLD_COMMIT is not a commit here" >&2; exit 2; }
SHORT=${COMMIT:0:8}
OLD_DIR=$WORK/$SHORT
OLD_BIN=$OLD_DIR/ferrum
if [ ! -x "$OLD_BIN" ]; then
  mkdir -p "$WORK"
  free_kb=$(df -k "$WORK" | awk 'NR == 2 { print $4 }')
  [ "$free_kb" -ge $((8 * 1024 * 1024)) ] || { echo "refused: building $SHORT needs about 3 GB, and less than 8 GB is free" >&2; exit 2; }
  step "building the previous build $SHORT in $OLD_DIR"
  rm -rf "$OLD_DIR/src"
  mkdir -p "$OLD_DIR/src"
  git -C "$ROOT" archive "$COMMIT" | tar -x -C "$OLD_DIR/src"
  # A build directory of its own: one shared with this checkout's would hold one artifact for both trees' crates
  # (cargo names a local crate's artifacts the same whatever its path), and the next build here would take the older one.
  (cd "$OLD_DIR/src" && CARGO_TARGET_DIR="$OLD_DIR/target" CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
    BUILD_DATE=$(git -C "$ROOT" log -1 --format=%ct "$COMMIT") cargo build --offline --locked -p ferrum-server --no-default-features --bin ferrum)
  cp "$OLD_DIR/target/debug/ferrum" "$OLD_BIN"
  rm -rf "$OLD_DIR/target"
fi
[ -d "$OLD_DIR/src/server/config" ] || { echo "refused: $OLD_DIR/src/server/config is missing; delete $OLD_DIR to build it again" >&2; exit 2; }

for p in "$P1" "$P2" "$PS"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$p (set ONEINPUT_PORTS)" >&2; exit 2; fi
done
work=$(mktemp -d)
pids=()
cleanup() {
  status=$?
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  sleep 1
  for pid in "${pids[@]}"; do kill -9 "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; done
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log "$work"/*.err; do [ -f "$log" ] && { echo "--- $log"; grep -v '^ferrum-getenv ' "$log" | tail -25; }; done
  fi
  sql postgres "drop database if exists $DB with (force)" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

# The getenv library: every name asked for, and whether it was set, to standard error; never a value.
cat >"$work/getenv.c" <<'C'
#ifndef __APPLE__
#define _GNU_SOURCE
#include <dlfcn.h>
#endif
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
static char *traced(const char *name) {
#else
char *getenv(const char *name) {
#endif
#ifdef __APPLE__
  char *v = getenv(name);
#else
  static char *(*real)(const char *);
  if (!real) real = (char *(*)(const char *))dlsym(RTLD_NEXT, "getenv");
  char *v = real(name);
#endif
  char line[600];
  size_t n = name ? strlen(name) : 0;
  if (n > 500) n = 500;
  memcpy(line, "ferrum-getenv ", 14);
  if (n) memcpy(line + 14, name, n);
  const char *said = v ? " set\n" : " unset\n";
  size_t m = strlen(said);
  memcpy(line + 14 + n, said, m);
  ssize_t ignored = write(2, line, 14 + n + m);
  (void)ignored;
  return v;
}
#ifdef __APPLE__
__attribute__((used)) static struct { const void *with; const void *of; } interposed[] __attribute__((section("__DATA,__interpose"))) = {
  {(const void *)traced, (const void *)getenv}};
#endif
C
if [ "$(uname)" = Darwin ]; then
  cc -dynamiclib -o "$work/getenv.dylib" "$work/getenv.c"
  TRACE="DYLD_INSERT_LIBRARIES=$work/getenv.dylib"
else
  cc -shared -fPIC -o "$work/getenv.so" "$work/getenv.c" -ldl
  TRACE="LD_PRELOAD=$work/getenv.so"
fi
# One process of this build, from its own server directory (its configuration files), with nothing but the address.
ferrum() { (cd "$ROOT/server" && exec env -i DATABASE_URL="$URL" "$TRACE" "$BIN" -e production "$@"); }
task() { local name=$1; shift; ferrum task "$@" 2>>"$work/$name.err"; }
start() { # name port: a replica of this build, in the background, until it answers
  (cd "$ROOT/server" && exec env -i DATABASE_URL="$URL" "$TRACE" "$BIN" -e production start) >"$work/$1.log" 2>"$work/$1.err" &
  pids+=($!)
  local pid=$!
  for _ in $(seq 1 180); do
    curl -fsS "http://127.0.0.1:$2/api/ready" >/dev/null 2>&1 && return 0
    kill -0 "$pid" 2>/dev/null || fail "the replica $1 stopped (its log follows)"
    sleep 1
  done
  fail "the replica $1 did not answer on $2 in 180 s"
}
last() { echo "${pids[${#pids[@]}-1]}"; }
stop() { local pid; pid=$(last); kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; unset "pids[${#pids[@]}-1]"; }

step "object storage: a private SeaweedFS with a key pair of its own, on 127.0.0.1:$PS"
KEY_ID=oneinput$(openssl rand -hex 6)
KEY=$(openssl rand -hex 24)
printf '{"identities":[{"name":"ferrum","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List","Tagging"]}]}' "$KEY_ID" "$KEY" >"$work/s3.json"
mkdir -p "$work/weed"
base=$((PS + 20000))
"$WEED" server -dir="$work/weed" -ip=127.0.0.1 -ip.bind=127.0.0.1 -master.port=$((base + 1)) -master.port.grpc=$((base + 11)) \
  -volume.port=$((base + 2)) -volume.port.grpc=$((base + 12)) -filer -filer.port=$((base + 3)) -filer.port.grpc=$((base + 13)) \
  -s3 -s3.port="$PS" -s3.port.grpc=$((base + 14)) -s3.config="$work/s3.json" -volume.max=4 >"$work/weed.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$PS/" && break; sleep 1; done
# The master's address with its gRPC port (`host:port.grpcPort`), as SeaweedFS names one off its default offset.
for _ in $(seq 1 30); do
  echo "s3.bucket.create -name ferrum-oneinput" | "$WEED" shell -master="127.0.0.1:$((base + 1)).$((base + 11))" 2>&1 \
    | grep -q "created bucket\|already exists" && break
  sleep 1
done

step "a fresh database, migrated by \`ferrum task migrate\` with DATABASE_URL alone"
sql postgres "drop database if exists $DB with (force)"
sql postgres "create database $DB owner ferrum"
task migrate migrate >"$work/migrate.log" || fail "migrate"

step "the settings, applied from a document"
cat >"$work/settings.yaml" <<YAML
installation:
  port: "$P1"
  binding: 127.0.0.1
  public_url: http://127.0.0.1:$P1
  log_level: warn
  s3_bucket: ferrum-oneinput
  s3_endpoint: http://127.0.0.1:$PS
  s3_region: us-east-1
  session_days: "14"
YAML
task apply_settings apply_settings file:"$work/settings.yaml" >"$work/apply.log" || fail "apply_settings"

step "the secrets, set by tasks: the object store's keys from files, two tokens made and shown once"
printf '%s' "$KEY_ID" >"$work/key_id"
printf '%s' "$KEY" >"$work/key"
task secret_set secret_set name:aws_access_key_id file:"$work/key_id" >/dev/null || fail "aws_access_key_id"
task secret_set secret_set name:aws_secret_access_key file:"$work/key" >/dev/null || fail "aws_secret_access_key"
shown() { awk '/shown this once/ {getline; getline; print $1; exit}'; }
BOOTSTRAP=$(task secret_set secret_set name:bootstrap_token | shown)
OPERATOR=$(task secret_set secret_set name:operator_token | shown)
[ -n "$BOOTSTRAP" ] && [ -n "$OPERATOR" ] || fail "the tokens were not shown"

step "two replicas: one on the port the document gave ($P1), one on the port a setting then gave ($P2)"
start one "$P1"
task set set key:port value:"$P2" >/dev/null || fail "set port"
start two "$P2"
A="http://127.0.0.1:$P1" B="http://127.0.0.1:$P2"
PASSWORD='One-Input-Passw0rd'
curl -fsS "$A/api/tenants" -H "x-ferrum-bootstrap-token: $BOOTSTRAP" -H 'content-type: application/json' \
  -d "{\"slug\":\"one\",\"name\":\"One\",\"admin_email\":\"admin@one.test\",\"admin_password\":\"$PASSWORD\",\"packages\":[\"contacts\"]}" >/dev/null \
  || fail "a workspace made with the bootstrap token"

step "a sign-in, a record saved on one replica and read on the other"
T=$(curl -fsS "$A/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"one\",\"email\":\"admin@one.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]')
[ "$(curl -sS -o "$work/contact.json" -w '%{http_code}' "$A/api/v1/Contact" -H "authorization: Bearer $T" -H 'content-type: application/json' -d '{"first_name":"Una"}')" = 201 ] \
  || fail "a record: $(cat "$work/contact.json")"
RID=$(json 'd.get("id") or d.get("name")' <"$work/contact.json")
curl -fsS "$B/api/v1/Contact?limit=10" -H "authorization: Bearer $T" -H 'accept-encoding: gzip' --compressed | grep -q '"Una"' || fail "the other replica reads it"

step "a file kept in object storage and read back"
printf 'one input, two stores\n' >"$work/note.txt"
FILE=$(curl -fsS "$A/api/files" -H "authorization: Bearer $T" -F entity=Contact -F record_id="$RID" -F file=@"$work/note.txt" | json 'd["url"]')
curl -fsS "$B$FILE" -H "authorization: Bearer $T" -o "$work/back.txt" || fail "read back"
cmp -s "$work/note.txt" "$work/back.txt" || fail "the file came back changed"
curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PS/ferrum-oneinput/" | grep -q 403 || fail "the bucket answers anyone"

step "a background job done once, with both replicas running background work"
curl -fsS -o /dev/null "$A/api/admin/export" -X POST -H "authorization: Bearer $T" || fail "an export"
exports="from job where kind like '%export%'"
for _ in $(seq 1 60); do [ "$(sql "$DB" "select count(*) $exports and status = 'done'")" -ge 1 ] && break; sleep 1; done
[ "$(sql "$DB" "select string_agg(status || ':' || attempts, ',') $exports")" = "done:1" ] || fail "the export: $(sql "$DB" "select string_agg(kind || ' ' || status || ':' || attempts, ', ') from job")"

step "a setting that applies at once, changed: both replicas take it"
task set set key:session_days value:7 >/dev/null || fail "set session_days"
said() { curl -fsS "$1/api/operator/settings" -H "x-ferrum-operator-token: $OPERATOR" -H 'accept-encoding: gzip' --compressed | json '[s["value"] for s in d["settings"] if s["key"] == "session_days"][0]'; }
t0=$(date +%s)
until [ "$(said "$A")" = 7 ] && [ "$(said "$B")" = 7 ]; do
  [ $(($(date +%s) - t0)) -le 30 ] || fail "the replicas did not take it within 30 s ($(said "$A"), $(said "$B"))"
  sleep 0.5
done
echo "    both replicas took it within $(($(date +%s) - t0)) s"

step "a restart: the session from before still works"
stop
start two-again "$P2"
[ "$(curl -sS -o /dev/null -w '%{http_code}' "$B/api/auth/me" -H "authorization: Bearer $T")" = 200 ] || fail "the session after a restart"

step "the doctor: the environment decides nothing"
task doctor doctor >"$work/doctor.txt" 2>&1 || true
grep -qE "^ok +settings.environment +no setting comes from the environment" "$work/doctor.txt" || { cat "$work/doctor.txt"; fail "the doctor"; }
grep -qE "^ok +secrets.environment +the vault decides every one" "$work/doctor.txt" || { cat "$work/doctor.txt"; fail "the doctor on secrets"; }
grep -E "^(ok|warning|BLOCKER) +(settings|secrets)\.environment" "$work/doctor.txt" | sed 's/^/    /'
stop
stop

step "what this build read from its environment"
python3 - "$ROOT" "$work" <<'PY' || fail "the environment"
import json, pathlib, re, subprocess, sys
root, work = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
read, found = set(), set()
for f in work.glob("*.err"):
    for line in f.read_text(errors="ignore").splitlines():
        if line.startswith("ferrum-getenv "):
            _, name, state = line.split(" ")
            read.add(name)
            if state == "set":
                found.add(name)
known = json.loads(subprocess.run([sys.executable, str(root / "scripts/settings.py"), "--names"], capture_output=True, text=True, check=True).stdout)
loader = {"DYLD_INSERT_LIBRARIES", "LD_PRELOAD"}
# What the operating system's own libraries ask for as the process runs (macOS's frameworks and dyld, glibc and its
# resolver), whoever calls them: their namespaces, not the program's.
system = re.compile(
    r"^(CF|__CF|NS|OBJC_|OS_ACTIVITY|OS_LOG|XPC_|LIBDISPATCH_|DISPATCH_|_?Malloc|__TRI|LIBTRACE_|DYLD_|APPLE_|IPHONE_|"
    r"NEXT_ROOT$|FEATUREFLAGS_|SYSTEM_VERSION_COMPAT$|XBS_|DNSSD_|RES_|TZFILE$|ACTIVITY_LOG_STDERR$|OAKeepAllocationStatistics$|"
    r"SYSINFO_CONF_ENABLE$|LOCALDOMAIN$|HOSTALIASES$|LC_|LANGUAGE$|MALLOC_|GCONV_PATH$|TZDIR$|GLIBC_TUNABLES$|LD_)"
)
read = {n for n in read if not system.match(n)}
wrong = []
if "DATABASE_URL" not in read:
    wrong.append("DATABASE_URL was never read: the trace sees nothing")
if found - {"DATABASE_URL"} - loader:
    wrong.append(f"found set besides DATABASE_URL: {sorted(found - {'DATABASE_URL'} - loader)}")
unknown = read - {"DATABASE_URL"} - loader - set(known["registry"]) - set(known["baseline"])
if unknown:
    wrong.append(f"read, and declared neither by the registry nor by the baseline in scripts/settings.py: {sorted(unknown)}")
legacy = sorted(read & set(known["registry"]) - set(known["baseline"]))
staying = sorted(read & set(known["baseline"]))
print(f"    {len(read)} names asked for; set: {sorted(found - loader)}")
print(f"    the registry's, read through this release only so that one set there wins: {len(legacy)}")
print(f"    the baseline's, which stay: {', '.join(staying)}")
for w in wrong:
    print(f"    {w}")
sys.exit(1 if wrong else 0)
PY

step "the previous build ($SHORT) on the same database"
(cd "$OLD_DIR/src/server" && exec env -i DATABASE_URL="$URL" "$OLD_BIN" -e production start) >"$work/previous.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 180); do curl -fsS "$B/api/ready" >/dev/null 2>&1 && break; kill -0 "$(last)" 2>/dev/null || fail "the previous build did not start"; sleep 1; done
curl -fsS "$B/api/ready" >/dev/null || fail "the previous build did not answer"
T2=$(curl -fsS "$B/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"one\",\"email\":\"admin@one.test\",\"password\":\"$PASSWORD\"}" | json 'd["token"]')
curl -fsS "$B/api/v1/Contact?limit=10" -H "authorization: Bearer $T2" | grep -q '"Una"' || fail "the previous build reads the record"
curl -fsS "$B$FILE" -H "authorization: Bearer $T2" -o "$work/back2.txt" && cmp -s "$work/note.txt" "$work/back2.txt" || fail "the previous build reads the file"
stop
echo "one input: the server ran on DATABASE_URL alone, twice, and the previous build after it"
