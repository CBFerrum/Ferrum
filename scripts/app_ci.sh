#!/bin/bash
# An app's CI on this machine, as its .github/workflows/ci.yml runs it on GitHub:
#   cargo fmt --check, cargo clippy --all-targets -D warnings, and cargo test against a Postgres
#   test database owned by the non-superuser `ferrum` role (superusers bypass row-level security);
#   then its interface's typecheck, tests and build, and the dist/ it commits checked against them.
#
# Usage: scripts/app_ci.sh <app checkout>
#   PLATFORM  the Ferrum checkout the app builds against (default: this one), through a git-ignored
#             .cargo/config.toml patch; without one already there, the script writes it
#   FRONTEND  a Ferrum-Frontend checkout with node_modules (default: <platform>/web)
#   PG        Postgres, as the `ferrum` role (default: postgres://ferrum:ferrum@localhost:5432)
#   NO_DEBUG  1 builds the tests without debug info in a target of their own, deleted afterwards:
#             a fraction of the disk, for a machine short of it
# The test binaries it makes are removed when it is done. Exit 0 when every step passed.
set -uo pipefail
APP_DIR=$(cd "${1:?an app checkout}" && pwd)
PLATFORM=${PLATFORM:-$(cd "$(dirname "$0")/.." && pwd)}
FRONTEND=${FRONTEND:-$PLATFORM/web}
PG=${PG:-postgres://ferrum:ferrum@localhost:5432}
APP=$(basename "$APP_DIR" | sed 's/^ferrum-app-//')
# Each test database its workflow names (FERRUM_..._TEST_DATABASE_URL), as one of this machine's
# own: the name with _localci, so another run's database is never reset.
DBS=()
while read -r var db; do DBS+=("$var=$PG/${db}_localci"); done < <(
  sed -n 's#^ *\(FERRUM_[A-Z0-9_]*_TEST_DATABASE_URL\): *postgres://[^/]*/\([a-z0-9_]*\) *$#\1 \2#p' "$APP_DIR/.github/workflows/ci.yml" 2>/dev/null)
[ ${#DBS[@]} -gt 0 ] || DBS=("FERRUM_$(echo "$APP" | tr 'a-z-' 'A-Z_')_TEST_DATABASE_URL=$PG/ferrum_${APP//-/_}_localci")
for kv in "${DBS[@]}"; do case "${kv##*/}" in *ferrum_dev*|*walk*|ferrum_test) echo "refusing to reset ${kv##*/}"; exit 2;; esac; done
TARGET=${CARGO_TARGET_DIR:-$PLATFORM/target}
export CARGO_INCREMENTAL=0
fail=0
step() { echo "== $1"; }
quiet() { grep -v "patch.*was not used\|Check that the patched\|^warning: Patch"; }
# A test run's outcome: its results when it passed, and its whole tail when it did not.
tests() {
  local log
  log=$(mktemp "${TMPDIR:-/tmp}/app-ci-test.XXXXXX")
  "$@" > "$log" 2>&1
  local code=$?
  if [ $code = 0 ]; then grep -E "^test result" "$log"; else quiet < "$log" | grep -v "^ *Compiling\|^ *Checking" | tail -60; fi
  rm -f "$log"
  return $code
}

cd "$APP_DIR" || exit 2
if [ ! -f .cargo/config.toml ]; then
  mkdir -p .cargo
  {
    echo "# Local development only (git-ignored): build against the platform checkout."
    echo '[patch."https://github.com/CBFerrum/Ferrum"]'
    for c in kernel meta domain calc apps ops; do echo "ferrum-$c = { path = \"$PLATFORM/crates/$c\" }"; done
  } > .cargo/config.toml
fi
git check-ignore -q .cargo/config.toml || { echo ".cargo/config.toml is not ignored here: refusing to leave it"; exit 2; }

step fmt
cargo fmt -- --check || fail=1

clippy() {
  step clippy
  CARGO_TARGET_DIR=$TARGET cargo clippy -q --all-targets -- -D warnings 2>&1 | quiet
  [ "${PIPESTATUS[0]}" = 0 ] || fail=1
}
# Short of disk, the tests build first, in their own target, so clippy's outputs cannot crowd them.
[ "${NO_DEBUG:-}" = 1 ] || clippy

step "test (${DBS[*]##*/})"
for kv in "${DBS[@]}"; do db=${kv##*/}; psql "$PG/postgres" -q -c "drop database if exists $db" -c "create database $db owner ferrum" || fail=1; done
if [ "${NO_DEBUG:-}" = 1 ]; then
  T=$(mktemp -d "${TMPDIR:-/tmp}/app-ci-target.XXXXXX")
  tests env "${DBS[@]}" CARGO_TARGET_DIR="$T" CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --no-fail-fast || fail=1
  rm -rf "${T:?}"
else
  tests env "${DBS[@]}" CARGO_TARGET_DIR="$TARGET" cargo test --no-fail-fast || fail=1
  crate=$(grep -m1 '^name' Cargo.toml | sed 's/.*"\(.*\)".*/\1/' | tr '-' '_')
  for t in tests/*.rs; do rm -f "$TARGET/debug/deps/$(basename "$t" .rs)"-[0-9a-f]*; done
  find "$TARGET/debug/deps" -maxdepth 1 -name "${crate}-[0-9a-f]*" -type f -perm -u+x -delete 2>/dev/null
fi
for kv in "${DBS[@]}"; do psql "$PG/postgres" -q -c "drop database if exists ${kv##*/}"; done
[ "${NO_DEBUG:-}" = 1 ] && clippy

step ui
# The interface builds the kit from ../../ferrum/web, so it is built in a layout that has one.
W=$(mktemp -d "${TMPDIR:-/tmp}/app-ci-ui.XXXXXX")
mkdir -p "$W/ferrum" "$W/app" && ln -s "$FRONTEND" "$W/ferrum/web"
rsync -a --exclude node_modules --exclude dist "$APP_DIR/ui/" "$W/app/ui/" && cp "$APP_DIR/app.yaml" "$W/app/"
ln -s "$FRONTEND/node_modules" "$W/app/ui/node_modules"
(cd "$W/app/ui" && node ../../ferrum/web/packages/build/cli.mjs typecheck && node ../../ferrum/web/packages/build/cli.mjs test 2>&1 | grep -E "Tests |FAIL" && node ../../ferrum/web/packages/build/cli.mjs build > /dev/null) || fail=1
diff -rq "$W/app/ui/dist" "$APP_DIR/ui/dist" > /dev/null || { echo "the committed ui/dist is not what its sources build with $FRONTEND"; fail=1; }
rm -rf "${W:?}"

echo "== $( [ $fail = 0 ] && echo PASS || echo FAIL ): $APP"
exit $fail
