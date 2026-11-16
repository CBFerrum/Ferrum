#!/usr/bin/env bash
# Ferrum's CI on this machine, as .github/workflows/ci.yml runs it on GitHub, job by job (scripts/app_ci.sh does the
# same for an app's repository). One line per step, PASS or FAIL; exit 1 when any step failed.
#
#   scripts/local_ci.sh              every job below
#   scripts/local_ci.sh rust audit   just those
#
# Jobs:
#   rust    cargo fmt --check; clippy over the workspace with the job's feature; the tests: every crate's unit tests,
#           then each package's integration suites a few at a time, then the doc tests, all with the job's feature (the
#           features the workspace build resolves, as CI's single `cargo test --workspace` does). Each suite's database
#           is the workflow's (or the suite's own default) with LOCALCI_SUFFIX, made afresh and dropped at the end.
#   saml    SAML built with xmlsec1: clippy, the crate's tests, and the kernel's saml_it, saml_keycloak_it and scan_it.
#           XMLSEC_PREFIX: an xmlsec1 install to build against instead of the one on PATH (e.g. 1.2.37, the image's,
#           built from source); its OpenSSL is taken from it unless OPENSSL_DIR is set. When the xmlsec1 differs from
#           the last run's, samael is built again: its bindings come from the headers, and nothing else tells cargo.
#   audit   cargo audit (cargo install cargo-audit --locked) and the job's scripts (pyyaml, helm, node).
#   move    the server without apps, and scripts/move_rehearsal.sh on databases of its own: an installation moved, one
#           workspace moved between two running servers and back, a sandbox, all as the servers' own role with no grant
#           on session_replication_role (D7).
#   replica not in the workflow, and only when named: a private primary and standby (scripts/replica_pair.sh, PostgreSQL
#           17's programs) and the kernel's replica_pair_it on them: real lag, a stopped standby, a cancelled query, a
#           promotion.
#   managed not in the workflow, and only when named: a private PostgreSQL that behaves like a managed service
#           (scripts/managed_pg.sh: an ordinary role owning only its databases, no grant on session_replication_role,
#           the extensions made by an administrator, PgBouncer in transaction mode with PostgreSQL beside it, TLS with
#           its own certificate authority). The server binary starts on it through the pooler with only a direct host,
#           and the suites of moves, sandboxes, taking a workspace back, deleting one, copies without the grant, the
#           restore marker, the dump and restore, the listeners and the session locks run against it through the
#           pooler, each database's direct address by host (FERRUM_TEST_DATABASE_DIRECT_HOST).
#   window  not in the workflow, and only when named: scripts/window_rehearsal.sh, an older build of main (WINDOW_OLD,
#           default 8a34644e, the first with floors) and this one on one database. The older build is made once and kept
#           in WINDOW_WORK (about 3 GB while it builds; refused under 8 GB free).
#   objects not in the workflow, and only when named: scripts/object_store_rehearsal.sh, the server without apps on real
#           S3 servers (SeaweedFS from Homebrew, run by deploy/store/start.sh, and moto 5.0.28 in OBJECT_PYTHON): a new
#           installation refused on a directory, files moved with files_move killed partway, two replicas, the doctor's
#           bucket lines, the restore drill, direct links and a machine's identity in place of keys.
#   oneinput not in the workflow, and only when named: scripts/one_input.sh, the server with its apps started with
#           DATABASE_URL in its environment and nothing else, as two replicas doing real work on a fresh database with
#           a private SeaweedFS for its files, Comms taking its meeting address from the table and signing with the
#           secret in its vault, every name it asks its environment for checked against the settings' registry and
#           scripts/settings.py's baseline; then the previous build (the merge base without apps, made once in
#           WINDOW_WORK as for `window`) on the same database. Needs SeaweedFS's `weed` and a C compiler.
#   companions not in the workflow, and only when named: scripts/companions_rehearsal.sh, the server without apps
#           rendering the bundled store's keys from a fresh database with DATABASE_URL alone (two renders at once, one
#           key pair), SeaweedFS started on the file by deploy/store/start.sh, servers writing through it, a rotation's
#           overlap and its end, and a vault that does not open writing nothing. LiveKit on rendered files is the
#           kernel's companions_it, in the rust job. Needs SeaweedFS's `weed`.
#   editors not run here: it needs Docker and Playwright's browsers. The image job and the cluster workflow need
#           Docker too: docs/operations.md, "Running the image and cluster checks on your own machine".
#
# Settings: PG (Postgres as the `ferrum` role; default postgres://ferrum:ferrum@localhost:5432), LOCALCI_SUFFIX (added to
# every database name; default _localci, so another run's database is never reset), NO_DEBUG=1 (tests without debug
# info: a fraction of the disk), CARGO_TARGET_DIR, LOCALCI_LOGS (where each step's log goes), LOCALCI_MIN_GB (stop below
# this much free disk; default 5). Passed through when set, as the workflow's services provide them: FERRUM_TEST_SUPERUSER
# (default postgres://$USER@localhost:5432), FERRUM_KMS_TEST_TRANSIT and FERRUM_KMS_TEST_TOKEN (OpenBao),
# FERRUM_TEST_S3_ENDPOINT and FERRUM_TEST_S3_BUCKET (moto), FERRUM_SAML_KEYCLOAK (Keycloak), FERRUM_TEST_CLAMD (ClamAV),
# MOVE_ADMIN_URL. Without them those parts say they skipped.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
PG=${PG:-postgres://ferrum:ferrum@localhost:5432}
SFX=${LOCALCI_SUFFIX:-_localci}
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
LOGS=${LOCALCI_LOGS:-$(mktemp -d "${TMPDIR:-/tmp}/ferrum-local-ci.XXXXXX")}
MIN_GB=${LOCALCI_MIN_GB:-5}
FEATURE=ferrum-server/control
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$TARGET
[ "${NO_DEBUG:-}" = 1 ] && export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
[[ "$SFX" =~ ^_[a-z0-9_]+$ ]] || { echo "LOCALCI_SUFFIX must be _ and lower-case letters, digits or _"; exit 2; }
mkdir -p "$LOGS"
failed=0
echo "logs: $LOGS"

# One step: its command's output to a log of its own, and a line saying how it went.
run() {
  local name=$1 log t0 code
  shift
  log="$LOGS/$(echo "$name" | tr -c 'a-zA-Z0-9_.-' '_').log"
  t0=$(date +%s)
  "$@" >"$log" 2>&1
  code=$?
  report "$name" "$code" "$(($(date +%s) - t0))" "$log"
  return "$code"
}
report() { # name code seconds log [detail]
  if [ "$2" = 0 ]; then
    echo "PASS $1 (${3}s)${5:+: $5}"
  else
    failed=1
    echo "FAIL $1 (${3}s)${5:+: $5}; log: $4"
    grep -E "^(error|FAILED|failures:|---- |thread '.*' panicked|test result: FAILED)" "$4" | head -20 | sed 's/^/     /'
  fi
}
room() {
  local free
  free=$(df -Pk "$TARGET" 2>/dev/null | awk 'NR==2 {print int($4 / 1048576)}')
  [ -z "$free" ] || [ "$free" -ge "$MIN_GB" ] && return 0
  failed=1
  echo "FAIL disk: ${free} GB free under $TARGET, less than $MIN_GB; stopping"
  exit 1
}
# The test executables a log's run used, deleted: hundreds of MB each.
tidy() { grep -oE 'deps/[a-z0-9_]+-[0-9a-f]{16}\)' "$1" | tr -d ')' | sort -u | while read -r p; do rm -f "$TARGET/debug/$p"; done; }

# Every suite's database: the workflow's, and each one a suite reads with a default of its own.
databases() {
  python3 - <<'PY'
import pathlib, re
pairs = {}
for p in [*pathlib.Path("crates").rglob("*.rs"), *pathlib.Path("server").rglob("*.rs")]:
    for m in re.finditer(r'"(FERRUM_[A-Z0-9_]*TEST_DATABASE_URL)"[\s\S]{0,160}?(?:postgres://[^/"\s]+/|,\s*")([a-z0-9_]+)"', p.read_text(errors="ignore")):
        pairs.setdefault(m.group(1), m.group(2))
for m in re.finditer(r'^\s*(FERRUM_[A-Z0-9_]*TEST_DATABASE_URL): postgres://[^/]+/([a-z0-9_]+)\s*$', pathlib.Path(".github/workflows/ci.yml").read_text(), re.M):
    pairs[m.group(1)] = m.group(2)
for var, db in sorted(pairs.items()):
    print(var, db)
PY
}
DBENV=()
while read -r var db; do DBENV+=("$var=$PG/$db$SFX"); done < <(databases)
fresh_databases() {
  local kv
  for kv in "${DBENV[@]}"; do
    psql "$PG/postgres" -v ON_ERROR_STOP=1 -q -c "drop database if exists ${kv##*/} with (force)" -c "create database ${kv##*/} owner ferrum" || return 1
  done
}
drop_databases() { local kv; for kv in "${DBENV[@]}"; do psql "$PG/postgres" -q -c "drop database if exists ${kv##*/} with (force)" >/dev/null 2>&1; done; }

# cargo test as the rust job runs it, limited by `args`, in the suites' databases; the binaries it ran then deleted.
tests() { # name args...
  local name=$1 log t0 code summary
  shift
  room
  log="$LOGS/$(echo "$name" | tr -c 'a-zA-Z0-9_.-' '_').log"
  t0=$(date +%s)
  env "${DBENV[@]}" cargo test --workspace --locked --features "$FEATURE" --no-fail-fast "$@" >"$log" 2>&1
  code=$?
  summary=$(awk '/^test result:/ {n++; p += $4; f += $6; if ($3 != "ok.") bad++} END {printf "%d suites, %d tests passed, %d failed", n, p, f}' "$log")
  report "$name" "$code" "$(($(date +%s) - t0))" "$log" "$summary"
  tidy "$log"
}

job_rust() {
  run "rust fmt" cargo fmt --all -- --check
  run "rust clippy" cargo clippy --workspace --all-targets --locked --features "$FEATURE" -- -D warnings
  if ! run "rust databases (${#DBENV[@]})" fresh_databases; then return; fi
  for v in FERRUM_KMS_TEST_TRANSIT FERRUM_TEST_S3_ENDPOINT; do
    [ -n "${!v:-}" ] || echo "     note: $v is not set: the suites that need it (OpenBao, an S3 endpoint in the workflow) skip that part"
  done
  tests "rust test: unit tests of every crate" --lib
  tests "rust test: the server binary" --bins
  local pkg names batch i
  while read -r pkg names; do
    batch=6
    [ "$pkg" = ferrum-server ] && batch=2
    read -r -a list <<<"$names"
    for ((i = 0; i < ${#list[@]}; i += batch)); do
      local args=()
      for t in "${list[@]:i:batch}"; do args+=(--test "$t"); done
      tests "rust test: $pkg ${list[*]:i:batch}" "${args[@]}"
    done
  done < <(cargo metadata --no-deps --format-version 1 --locked 2>/dev/null | python3 -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    t = [x["name"] for x in p["targets"] if "test" in x["kind"]]
    if t:
        print(p["name"], " ".join(t))')
  tests "rust test: doc tests" --doc
  drop_databases
}

job_saml() {
  if [ -n "${XMLSEC_PREFIX:-}" ]; then
    export PATH="$XMLSEC_PREFIX/bin:$PATH" PKG_CONFIG_PATH="$XMLSEC_PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  fi
  # The Rust crates must link the very libraries this xmlsec1 was built against: two copies of libxml2 or of OpenSSL in
  # one process crash (seen on macOS, where pkg-config finds the system's libxml2 and Homebrew's xmlsec1 uses its own).
  # xmlsec1-config names their folders.
  local dir
  for dir in $(xmlsec1-config --libs 2>/dev/null | tr ' ' '\n' | sed -n 's#^-L##p'); do
    [ -d "$dir/pkgconfig" ] && export PKG_CONFIG_PATH="$dir/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  done
  if [ -z "${OPENSSL_DIR:-}" ]; then
    OPENSSL_DIR=$(xmlsec1-config --libs 2>/dev/null | tr ' ' '\n' | sed -n 's#^-L\(.*openssl[^/]*\)/lib$#\1#p' | head -1)
    [ -n "$OPENSSL_DIR" ] && export OPENSSL_DIR
  fi
  local version stamp="$TARGET/.localci-xmlsec"
  version=$(xmlsec1-config --version 2>/dev/null) || { report "saml xmlsec1" 1 0 /dev/null "no xmlsec1-config on PATH"; return; }
  echo "     xmlsec1 $version at $(command -v xmlsec1-config)"
  if [ "$(cat "$stamp" 2>/dev/null)" != "$version $(command -v xmlsec1-config)" ]; then
    run "saml samael built again for this xmlsec1" cargo clean -p samael -p ferrum-saml
    mkdir -p "$TARGET" && echo "$version $(command -v xmlsec1-config)" >"$stamp"
  fi
  run "saml clippy" cargo clippy -p ferrum-saml -p ferrum-kernel --features ferrum-saml/xmlsec,ferrum-kernel/saml --all-targets --locked -- -D warnings
  room
  local log="$LOGS/saml_crate.log" t0
  t0=$(date +%s)
  cargo test -p ferrum-saml --features xmlsec --locked >"$log" 2>&1
  report "saml test: ferrum-saml on xmlsec1 $version" $? "$(($(date +%s) - t0))" "$log" "$(grep -E '^test result' "$log" | head -1)"
  tidy "$log"
  if ! run "saml databases" fresh_databases; then return; fi
  [ -n "${FERRUM_SAML_KEYCLOAK:-}" ] || echo "     note: FERRUM_SAML_KEYCLOAK is not set: saml_keycloak_it skips (the workflow runs Keycloak 26.8.1)"
  [ -n "${FERRUM_TEST_CLAMD:-}" ] || echo "     note: FERRUM_TEST_CLAMD is not set: scan_it skips ClamAV (the workflow runs it)"
  room
  log="$LOGS/saml_kernel.log"
  t0=$(date +%s)
  env "${DBENV[@]}" cargo test -p ferrum-kernel --features saml --locked --no-fail-fast --test saml_it --test saml_keycloak_it --test scan_it >"$log" 2>&1
  report "saml test: saml_it, saml_keycloak_it, scan_it" $? "$(($(date +%s) - t0))" "$log" "$(grep -cE '^test result: ok' "$log") of 3 suites passed"
  tidy "$log"
  drop_databases
}

job_audit() {
  if cargo audit --version >/dev/null 2>&1; then run "audit cargo audit" cargo audit; else report "audit cargo audit" 1 0 /dev/null "cargo-audit is not installed (cargo install cargo-audit --locked)"; fi
  local base
  base=$(git merge-base HEAD origin/main 2>/dev/null || git rev-parse HEAD)
  run "audit registry, accepted advisories and generated documents" sh -c 'python3 scripts/registry.py check && python3 scripts/test_registry.py && python3 scripts/test_manifests.py && python3 scripts/mutants.py --check && python3 scripts/test_baseline.py && python3 scripts/test_mutation_paths.py && python3 scripts/test_builder_inventory.py'
  run "audit two-version window (since $(git rev-parse --short "$base"))" sh -c "python3 scripts/test_window.py && python3 scripts/window.py --range $base..HEAD"
  run "audit Helm chart against deploy/k8s" bash deploy/helm/test.sh
  run "audit Helm chart's deployment shapes" bash deploy/helm/test_pools.sh
  run "audit offline bundles" python3 deploy/offline/test_offline.py
  run "audit the bar runtime's pure helpers" node --test server/shell/runtime.test.mjs
  run "audit licenses" python3 scripts/licenses.py check
  run "audit settings' registry against what reads them" sh -c 'python3 scripts/test_settings.py && python3 scripts/settings.py'
  run "audit app contract" python3 scripts/check_apps.py
  run "audit direct writes reviewed" python3 scripts/mutation_paths.py
  run "audit secret paths" sh -c 'python3 scripts/test_secret_paths.py && python3 scripts/secret_paths.py'
}

job_move() {
  run "move build (no apps)" cargo build -p ferrum-server --no-default-features --bin ferrum --locked || return
  run "move rehearsal" scripts/move_rehearsal.sh "ferrum_moverehearsal_a$SFX" "ferrum_moverehearsal_b$SFX"
}

job_replica() {
  local dir code
  dir=$(mktemp -u "${TMPDIR:-/tmp}/ferrum-pair.XXXXXX")
  run "replica pair up" scripts/replica_pair.sh up "$dir" || { scripts/replica_pair.sh down "$dir" >/dev/null 2>&1; return; }
  # The addresses in a shell of their own; `run` has already said how it went, its code says whether it failed.
  (. "$dir/env" && run "replica test: replica_pair_it on a real standby" cargo test -p ferrum-kernel --locked --test replica_pair_it)
  code=$?
  run "replica pair down" scripts/replica_pair.sh down "$dir"
  [ "$code" = 0 ] || failed=1
}

job_managed() {
  local dir
  dir=$(mktemp -u "${TMPDIR:-/tmp}/ferrum-managed.XXXXXX")
  run "managed build (no apps)" cargo build -p ferrum-server --no-default-features --bin ferrum --locked || return
  run "managed up" scripts/managed_pg.sh up "$dir" || { scripts/managed_pg.sh down "$dir" >/dev/null 2>&1; return; }
  (
    failed=0
    . "$dir/env"
    read -r port bouncer <<<"$FERRUM_MANAGED_PORTS"
    # Every suite's database, made by the administrator and owned by the application's role.
    for kv in "${DBENV[@]}"; do
      scripts/managed_pg.sh db "$dir" "${kv##*/}" >/dev/null || exit 1
      export "${kv%%=*}=$FERRUM_MANAGED_BASE:$bouncer/${kv##*/}"
    done
    export FERRUM_TEST_DATABASE_DIRECT_HOST=127.0.0.1:$port FERRUM_TEST_SUPERUSER=${FERRUM_MANAGED_ADMIN_URL%/postgres}
    run "managed: the server starts through the pooler with only a direct host" scripts/managed_pg.sh serve "$dir" || failed=1
    managed_tests "managed: TLS with the service's certificate authority" -p ferrum-kernel --lib address_tests
    # This one takes PostgreSQL's own address and the pooler's beside it.
    (export FERRUM_TEST_POOLER_URL=$FERRUM_TEST_DATABASE_URL FERRUM_TEST_DATABASE_URL=$FERRUM_MANAGED_BASE:$port/${FERRUM_TEST_DATABASE_URL##*/}
      managed_tests "managed: listeners and session locks through the pooler" -p ferrum-kernel --test database_urls_it; exit $failed) || failed=1
    managed_tests "managed: copies without the grant, deleting a workspace, the dump and restore, starts together"       -p ferrum-kernel --test copy_ready_it --test closure_it --test dump_it --test boot_migrate_it
    managed_tests "managed: taking a workspace back, the restore marker, failovers" -p ferrum-kernel --test recovery_it
    managed_tests "managed: moving a workspace" -p ferrum-kernel --test workspace_move_it
    managed_tests "managed: sandboxes" -p ferrum-kernel --test sandbox_it
    # Two running servers as the application's role on PostgreSQL itself: an installation moved, a workspace moved
    # between them and back, a sandbox; the role makes the extensions itself there.
    run "managed: move rehearsal (an installation, a workspace there and back, a sandbox) with no grant"       env MOVE_ADMIN_URL="${FERRUM_MANAGED_ADMIN_URL%/postgres}" MOVE_APP_URL="$FERRUM_MANAGED_BASE:$port"       scripts/move_rehearsal.sh "ferrum_moverehearsal_a$SFX" "ferrum_moverehearsal_b$SFX" || failed=1
    # Last: its test binary builds the server with every app over target/debug/ferrum, which the steps above run
    # without apps.
    managed_tests "managed: the certificate authority through the server's whole start" -p ferrum-server --test database_ca_it
    exit $failed
  )
  [ $? = 0 ] || failed=1
  run "managed down" scripts/managed_pg.sh down "$dir"
}

# cargo test against the managed rehearsal's databases (the environment job_managed set), reported as `tests` does.
managed_tests() { # name args...
  local name=$1 log t0 code summary
  shift
  room
  log="$LOGS/$(echo "$name" | tr -c 'a-zA-Z0-9_.-' '_').log"
  t0=$(date +%s)
  cargo test --workspace --locked --features "$FEATURE" --no-fail-fast "$@" >"$log" 2>&1
  code=$?
  summary=$(awk '/^test result:/ {n++; p += $4; f += $6} END {printf "%d suites, %d tests passed, %d failed", n, p, f}' "$log")
  report "$name" "$code" "$(($(date +%s) - t0))" "$log" "$summary"
  [ "$code" = 0 ] || failed=1
}

job_window() {
  run "window build (no apps)" cargo build -p ferrum-server --no-default-features --bin ferrum --locked || return
  run "window rehearsal" scripts/window_rehearsal.sh "${WINDOW_OLD:-8a34644e}" "ferrum_windowrehearsal$SFX"
}

job_objects() {
  run "objects build (no apps)" cargo build -p ferrum-server --no-default-features --bin ferrum --locked || return
  run "objects rehearsal" scripts/object_store_rehearsal.sh "ferrum_objrehearsal$SFX"
}

job_oneinput() {
  run "oneinput build (with its apps)" cargo build -p ferrum-server --bin ferrum --locked || return
  run "oneinput proof" scripts/one_input.sh "ferrum_oneinput$SFX"
}

job_companions() {
  run "companions build (no apps)" cargo build -p ferrum-server --no-default-features --bin ferrum --locked || return
  run "companions rehearsal" scripts/companions_rehearsal.sh "ferrum_companions$SFX"
}

job_editors() { echo "SKIP editors: needs Docker (its nginx image) and Playwright's browsers; docs/operations.md says how to run it"; }

jobs=("$@")
[ ${#jobs[@]} -gt 0 ] || jobs=(rust saml audit move editors)
for j in "${jobs[@]}"; do
  case "$j" in
    # Each job in a shell of its own, so what one sets (saml's xmlsec1 and OpenSSL) never reaches the next.
    rust | saml | audit | move | editors | replica | window | managed | objects | oneinput | companions) echo "== $j"; (failed=0; "job_$j"; exit $failed) || failed=1 ;;
    *) echo "no job $j (rust, saml, audit, move, editors; and, only when named, replica, window, managed, objects, oneinput, companions)"; exit 2 ;;
  esac
done
echo "== $([ $failed = 0 ] && echo PASS || echo FAIL): ${jobs[*]}"
exit $failed
