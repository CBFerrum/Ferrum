#!/usr/bin/env bash
# Rehearses the vault's root key held outside the database by a real OpenBao (docs/operations.md, "The vault's key from a
# key-management service" and "The production profile"): this build's server on a throwaway database whose vault opens
# through OpenBao's Transit engine, and a private `bao server` on this machine, no Docker.
#
#   scripts/bao_rehearsal.sh dev  ferrum_baorehearsal_<name>     # bao server -dev: in memory, always unsealed
#   scripts/bao_rehearsal.sh file ferrum_baorehearsal_<name>     # a file-backed server, initialised and unsealed here
#
# The service is the only thing that holds a key: `bao server` is started on its own port with a data directory the script
# makes and marks (the machine's default instance, and any path outside that directory, are never touched), and it is
# stopped and the directory removed on the way out, also when a step fails. Nothing secret is kept anywhere but that
# directory: the tokens, the secret id and the recovery key are made at run time and go with it.
#
# Each step asserts a state, not a time; what the key service did is read from its own audit log.
# 1. A new installation whose settings name a key service takes the `kms` slot, not the credential slot; with a recovery
#    slot it is the production profile and the doctor says so (vault.root ok).
# 2. A server's start unwraps the slot through OpenBao: its audit log shows a decrypt on the key by the policy's token.
# 3. The Transit key is rotated: the slot's ciphertext (version 1) still opens, a start still opens the vault; the slot
#    made again (`vault_rotate level:slot kind:kms`) is wrapped by version 2, and the doctor, once the policy lets it read
#    the key, says so; `min_decryption_version` raised, the old ciphertext is refused by OpenBao and the new slot opens.
# 4. (file, not yet run) OpenBao is stopped: the running server keeps working (a sign-in, and a workspace made through it, which needs
#    keys of the vault) and a new start refuses to open the vault, saying the key-management service cannot be reached and
#    which slot it was: it does not open with another slot, there is none.
# 5. (file, not yet run) OpenBao comes back, unsealed: the next start opens. (dev: a restart forgets everything; the step is left out.)
# 6. The token's file replaced with a wrong one: refused, in a sentence that says it is a refusal and not an outage; with
#    the right one: opens, and nothing else was restarted (OpenBao's and the running server's processes are the same).
# 7. An AppRole instead of the token (kms_approle_role_id, the secret id in the file kms.secret-id): the same two ways, and
#    its logins are in OpenBao's audit log.
# 8. The policy narrowed so that decrypt is denied: refused, said plainly as a refusal; restored, it opens.
# 9. A weaker slot is never used silently: the credential slot added back beside the kms slot, the doctor warns that the
#    service protects nothing; with OpenBao stopped (file) a start opens with it and the log says the kms slot did not open.
#
# Settings: FERRUM_BIN (default ${CARGO_TARGET_DIR:-target}/debug/ferrum, built with `cargo build -p ferrum-server
# --no-default-features --bin ferrum`, a debug build, which reads the files of /run/ferrum from FERRUM_TEST_RUN_DIR),
# BAO (default `bao` on the PATH: brew install openbao), BAO_APP_URL (the servers' role, which may create databases;
# default postgres://ferrum:ferrum@localhost:5432), BAO_PORT (the first port: servers on it and the next three, OpenBao on
# it + 10; default 39500). Every process it starts is stopped on the way out, and its database dropped.
set -euo pipefail
PREFIX=ferrum_baorehearsal_
[ $# -eq 2 ] || { echo "usage: $0 dev|file ${PREFIX}<name>" >&2; exit 2; }
MODE=$1 NAME=$2
[[ "$MODE" == dev || "$MODE" == file ]] || { echo "usage: $0 dev|file ${PREFIX}<name>" >&2; exit 2; }
[[ "$NAME" == "$PREFIX"* && "$NAME" =~ ^[a-z0-9_]+$ ]] || { echo "refused: $NAME does not start with $PREFIX" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "$ROOT" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
BAO=${BAO:-bao}
APP=${BAO_APP_URL:-postgres://ferrum:ferrum@localhost:5432}
BASE=${BAO_PORT:-39500}
BAO_ADDR_PORT=$((BASE + 10))
[ -x "$BIN" ] || { echo "no server binary at $BIN (cargo build -p ferrum-server --no-default-features --bin ferrum)" >&2; exit 2; }
command -v "$BAO" >/dev/null || { echo "refused: no bao on the PATH (brew install openbao, or set BAO)" >&2; exit 2; }
for tool in psql curl python3; do command -v "$tool" >/dev/null || { echo "refused: no $tool on the PATH" >&2; exit 2; }; done
for port in "$BASE" $((BASE + 1)) $((BASE + 2)) $((BASE + 3)) "$BAO_ADDR_PORT" $((BAO_ADDR_PORT + 1)); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$port (set BAO_PORT)" >&2; exit 2; fi
done
# The directory everything of OpenBao's lives in; it is only ever removed when it carries the mark.
work=$(mktemp -d)
MARK=.ferrum-bao-rehearsal
: >"$work/$MARK"
mkdir -p "$work/bao" "$work/run" "$work/files"
BAO_PID='' SERVER_PID='' SERVED=''
export LC_ALL=C
TOKEN_FILE="$work/run/kms.token" SECRET_ID_FILE="$work/run/kms.secret-id"
BAO_URL="http://127.0.0.1:$BAO_ADDR_PORT"
FERRUM_ADDR="http://127.0.0.1:$BASE"

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# The vault's rows, as the vault's module reads them.
sql() { PGOPTIONS='-c ferrum.system=on -c ferrum.vault=on' psql "$APP/$NAME" -v ON_ERROR_STOP=1 -Atqc "$1"; }
end() { [ -z "$1" ] || { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }; }
cleanup() {
  status=$?
  end "$SERVER_PID"
  end "$BAO_PID"
  if [ "$status" -ne 0 ]; then
    for log in "$work"/*.log "$work"/bao/server.log; do [ -f "$log" ] && { echo "--- $log"; tail -25 "$log"; }; done
  fi
  psql "$APP/postgres" -qc "drop database if exists $NAME with (force)" >/dev/null 2>&1 || true
  [ -f "$work/$MARK" ] && rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
psql "$APP/postgres" -v ON_ERROR_STOP=1 -qc "drop database if exists $NAME with (force)" -c "create database $NAME owner ferrum" >/dev/null

# ── OpenBao: its own port and directory ──
# A command of the `bao` client, as the root token: it reads its address and token from these two variables alone, in a
# subshell, never from this one (a token helper file is only written by `bao login`, which is not used).
b() { (export BAO_ADDR="$BAO_URL" BAO_TOKEN="$ROOT_TOKEN"; "$BAO" "$@"); }
bao_up() {
  # The audit device is declared in the configuration (OpenBao 2.x does not enable one through its API).
  cat >"$work/bao/audit.hcl" <<EOF
audit "file" "rehearsal" {
  options {
    file_path = "$work/bao/audit.log"
  }
}
EOF
  if [ "$MODE" = dev ]; then
    ROOT_TOKEN=$(openssl rand -hex 16)
    (exec "$BAO" server -dev -config="$work/bao/audit.hcl" -dev-listen-address="127.0.0.1:$BAO_ADDR_PORT" -dev-root-token-id="$ROOT_TOKEN") >"$work/bao/server.log" 2>&1 &
  else
    cat >"$work/bao/bao.hcl" <<EOF
storage "file" { path = "$work/bao/data" }
listener "tcp" {
  address = "127.0.0.1:$BAO_ADDR_PORT"
  tls_disable = true
}
disable_mlock = true
EOF
    (exec "$BAO" server -config="$work/bao/bao.hcl" -config="$work/bao/audit.hcl") >>"$work/bao/server.log" 2>&1 &
  fi
  BAO_PID=$!
  for _ in $(seq 1 60); do
    # 200 unsealed, 501 not initialised, 503 sealed: all of them mean it is listening.
    code=$(curl -s -o /dev/null -w '%{http_code}' "$BAO_URL/v1/sys/health" || true)
    [ "$code" != 000 ] && return 0
    kill -0 "$BAO_PID" 2>/dev/null || fail "bao ended: $(tail -3 "$work/bao/server.log")"
    sleep 1
  done
  fail "bao did not come up"
}
bao_unseal() {
  if [ ! -f "$work/bao/init.json" ]; then
    (export BAO_ADDR="$BAO_URL"; "$BAO" operator init -key-shares=1 -key-threshold=1 -format=json) >"$work/bao/init.json" 2>"$work/bao/init.log" || fail "bao operator init"
    ROOT_TOKEN=$(json "d['root_token']" <"$work/bao/init.json")
  fi
  (export BAO_ADDR="$BAO_URL"; "$BAO" operator unseal "$(json "d['unseal_keys_b64'][0]" <"$work/bao/init.json")" >/dev/null) || fail "bao operator unseal"
}
bao_stop() { end "$BAO_PID"; BAO_PID=''; }
bao_down_says() { ! curl -s -o /dev/null "$BAO_URL/v1/sys/health"; }
POLICY_RW='path "transit/encrypt/ferrum" { capabilities = ["update"] }
path "transit/decrypt/ferrum" { capabilities = ["update"] }'
POLICY_READ="$POLICY_RW
path \"transit/keys/ferrum\" { capabilities = [\"read\"] }"
POLICY_ENCRYPT_ONLY='path "transit/encrypt/ferrum" { capabilities = ["update"] }'
policy() { printf '%s\n' "$1" | b policy write ferrum-kms - >/dev/null; }
# What OpenBao's own audit log says: how many requests of this path were made (as a token or a role of the policy).
audited() { python3 - "$work/bao/audit.log" "$1" <<'PY'
import json, sys
n = 0
for line in open(sys.argv[1]):
    e = json.loads(line)
    if e.get("type") == "request" and e["request"]["path"] == sys.argv[2]:
        n += 1
print(n)
PY
}
audited_policy() { python3 - "$work/bao/audit.log" "$1" <<'PY'
import json, sys
seen = set()
for line in open(sys.argv[1]):
    e = json.loads(line)
    if e.get("type") == "request" and e["request"]["path"] == sys.argv[2]:
        seen.add(tuple(sorted((e.get("auth") or {}).get("policies") or [])))
print(sorted(seen))
PY
}

step "OpenBao ($MODE) on 127.0.0.1:$BAO_ADDR_PORT, its directory a marked one under $(basename "$work"): Transit, a key, a policy of encrypt and decrypt on it alone, an AppRole"
bao_up
[ "$MODE" = dev ] || bao_unseal
b secrets enable transit >/dev/null
b write -f transit/keys/ferrum >/dev/null
policy "$POLICY_RW"
b auth enable approle >/dev/null
b write auth/approle/role/ferrum token_policies=ferrum-kms token_ttl=1h secret_id_num_uses=0 >/dev/null
ROLE_ID=$(b read -field=role_id auth/approle/role/ferrum/role-id)
SECRET_ID=$(b write -f -field=secret_id auth/approle/role/ferrum/secret-id)
umask 077
b token create -policy=ferrum-kms -ttl=4h -field=token >"$TOKEN_FILE"
GOOD_TOKEN=$(cat "$TOKEN_FILE")
printf '%s\n' "$SECRET_ID" >"$work/good.secret-id"
umask 022
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "x-vault-token: $GOOD_TOKEN" -X POST -d '{}' "$BAO_URL/v1/transit/keys/ferrum/rotate")" = 403 ] || fail "the policy allows more than encrypt and decrypt"

# ── Ferrum: this build, on the database, as an installation runs it ──
# Development, which may keep files in a directory (a new installation in production needs an object store, which this does not rehearse). The settings the server reads from its environment; the key service's token is only ever the file in $work/run.
ENVIRONMENT=(LOCO_ENV=development DATABASE_URL="$APP/$NAME" FERRUM_FILES_DIR="$work/files" FERRUM_REQUIRE_OBJECT_STORAGE=false LOG_LEVEL=info
  BINDING=127.0.0.1 PORT="$BASE" PUBLIC_URL="$FERRUM_ADDR" FERRUM_DEFAULT_WORKSPACE=acme FERRUM_BOOTSTRAP_TOKEN=rehearsal-token
  FERRUM_TEST_RUN_DIR="$work/run" FERRUM_KMS_TRANSIT="$BAO_URL/transit/ferrum")
task() { (cd "$ROOT/server" && exec env -i PATH="$PATH" "${ENVIRONMENT[@]}" "$BIN" task "$@"); }
# A server's start, in the background, on the port in START_PORT: 0 when it is ready (its pid in SERVED), 1 when it ended first
# (what it said is in $1.log). The first server stays up (on $BASE) while the others, which only prove the vault opens or
# does not, come and go on the next port.
START_PORT=$((BASE + 1))
try_start() {
  local log="$work/$1.log"
  (cd "$ROOT/server" && exec env -i PATH="$PATH" "${ENVIRONMENT[@]}" PORT="$START_PORT" PUBLIC_URL="http://127.0.0.1:$START_PORT" "$BIN" start) >"$log" 2>&1 &
  SERVED=$!
  for _ in $(seq 1 240); do
    curl -fsS "http://127.0.0.1:$START_PORT/api/ready" >/dev/null 2>&1 && return 0
    kill -0 "$SERVED" 2>/dev/null || { wait "$SERVED" 2>/dev/null || true; SERVED=''; return 1; }
    sleep 1
  done
  fail "a server neither became ready nor ended ($1)"
}
stop_served() { end "$SERVED"; SERVED=''; }
# A start that must open the vault, and one that must be refused, which says what it said.
opens() { try_start "$1" || fail "$2: the server did not start: $(grep -v '^\s*$' "$work/$1.log" | tail -3)"; grep -q "the vault is open" "$work/$1.log" || fail "$2: no 'the vault is open' in its log"; }
refused() { if try_start "$1"; then stop_served; fail "$2: the server started"; fi; }
slots() { sql "select string_agg(kind, ',' order by kind) from vault.root_slot"; }
wrapped_version() { sql "select split_part(kms_wrapped, ':', 2) from vault.root_slot where kind = 'kms'"; }
login() { curl -fsS "$FERRUM_ADDR/api/auth/login" -H 'content-type: application/json' -d "{\"tenant\":\"$1\",\"email\":\"admin@$1.test\",\"password\":\"Correct-Horse-9\"}" | json 'd["token"]'; }
doctor_line() { local all; all=$(task doctor 2>/dev/null) || true; printf '%s\n' "$all" | grep -E "(^|[^a-z.])$1([^a-z.]|$)" | head -1 || true; }

step "1. a new installation whose settings name the key service takes the kms slot, not the credential slot"
task migrate >"$work/migrate.log" 2>&1 || fail "migrate: $(tail -3 "$work/migrate.log")"
task create_tenant slug:acme name:Acme email:admin@acme.test password:Correct-Horse-9 >"$work/tenant.log" 2>&1 || fail "create_tenant: $(tail -3 "$work/tenant.log")"
[ "$(slots)" = kms ] || fail "the slots of a new installation: $(slots)"
[ "$(sql "select params::jsonb->>'how' || ' ' || (params::jsonb->>'key') from vault.root_slot where kind = 'kms'")" = "transit transit/ferrum" ] || fail "how the slot was made"
[ "$(wrapped_version)" = v1 ] || fail "the slot's ciphertext is not by version 1 of the key"
task vault_status format:json >"$work/status.json" 2>/dev/null || fail "vault_status"
[ "$(json "d['opened_with']" <"$work/status.json")" = kms ] || fail "a task did not open the vault with its kms slot"
task vault_add_slot kind:recovery >"$work/recovery.txt" 2>&1 || fail "the recovery slot: $(tail -3 "$work/recovery.txt")"
[ "$(slots)" = kms,recovery ] || fail "the slots with a recovery key: $(slots)"
line=$(doctor_line vault.root)
[[ "$line" == *"outside the database"* ]] || fail "the doctor on the production profile: $line"
token_line=$(doctor_line vault.root.token)
[[ "$token_line" == *"expires in"*"hours"* ]] || fail "the doctor on a token that runs out within a day: $token_line"
echo "    slots: kms, recovery, no credential slot; the doctor: ${line:0:140}"
echo "    and of the 4-hour token: ${token_line:0:120}"

step "2. a server's start unwraps through OpenBao"
before=$(audited transit/decrypt/ferrum)
START_PORT=$BASE
opens a "a start"
START_PORT=$((BASE + 1))
[ "$(audited transit/decrypt/ferrum)" -gt "$before" ] || fail "OpenBao saw no decrypt for the start"
[ "$(audited_policy transit/decrypt/ferrum)" = "[('default', 'ferrum-kms')]" ] || fail "who decrypted: $(audited_policy transit/decrypt/ferrum)"
SERVER_PID=$SERVED SERVED=''
[ "$(slots)" = kms,recovery ] || fail "a server's start changed the slots: $(slots)"
echo "    the start's decrypt is in OpenBao's audit log, by a token of the policy ferrum-kms alone"

step "3. the Transit key rotated: the old ciphertext still opens until the slot is made again; then not"
OLD_CIPHERTEXT=$(sql "select kms_wrapped from vault.root_slot where kind = 'kms'")
b write -f transit/keys/ferrum/rotate >/dev/null
[ "$(b read -field=latest_version transit/keys/ferrum)" = 2 ] || fail "the key is not at version 2"
[ "$(task vault_status format:json 2>/dev/null | json "d['opened_with']")" = kms ] || fail "the slot (version 1) no longer opens after a rotation"
echo "    key at version 2, the slot's ciphertext still version $(wrapped_version): a task opens the vault with it"
line=$(doctor_line vault.root.key)
[[ "$line" == *"were not read"* ]] || fail "with encrypt and decrypt alone the doctor says it could not read the versions: $line"
policy "$POLICY_READ"
line=$(doctor_line vault.root.key)
[[ "$line" == *"wrapped by version 1"*"which is at 2"* ]] || fail "the doctor, once it may read the key: $line"
task vault_rotate level:slot kind:kms >"$work/rotate.log" 2>&1 || fail "vault_rotate level:slot: $(tail -3 "$work/rotate.log")"
[ "$(wrapped_version)" = v2 ] || fail "the slot is not wrapped by version 2: $(wrapped_version)"
b write transit/keys/ferrum/config min_decryption_version=2 >/dev/null
if (export BAO_ADDR="$BAO_URL" BAO_TOKEN="$GOOD_TOKEN"; "$BAO" write transit/decrypt/ferrum ciphertext="$OLD_CIPHERTEXT") >"$work/old.log" 2>&1; then fail "OpenBao decrypted the old ciphertext"; fi
grep -qi "disallowed by policy\|too old" "$work/old.log" || fail "the old ciphertext was refused for another reason: $(tail -2 "$work/old.log")"
[ "$(task vault_status format:json 2>/dev/null | json "d['opened_with']")" = kms ] || fail "the new slot does not open"
line=$(doctor_line vault.root.key)
[[ "$line" == *"wrapped by version 2, the Transit key's latest"* ]] || fail "the doctor after the rotation: $line"
echo "    slot made again: version 2; min_decryption_version 2: the old ciphertext is refused by OpenBao, the new slot opens"
policy "$POLICY_RW"

if [ "$MODE" = file ]; then
  step "4. OpenBao stopped: the running server keeps working, a new start refuses and says why"
  bao_stop
  bao_down_says || fail "bao still answers"
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$FERRUM_ADDR/api/ready")" = 200 ] || fail "the running server is not ready with the key service away"
  TOKEN=$(login acme) || fail "a sign-in with the key service away"
  curl -fsS "$FERRUM_ADDR/api/tenants" -H 'x-ferrum-bootstrap-token: rehearsal-token' -H 'content-type: application/json' \
    -d '{"slug":"made-while-away","name":"Made while away","admin_email":"admin@made-while-away.test","admin_password":"Correct-Horse-9","packages":[]}' >/dev/null ||
    fail "a workspace made through the running server with the key service away"
  login made-while-away >/dev/null || fail "signing in to the workspace made while the key service was away"
  kill -0 "$SERVER_PID" || fail "the running server ended"
  refused b "a start with the key service away"
  grep -q "the vault: no slot opens the vault: the kms slot: the key-management service cannot be reached" "$work/b.log" || fail "the refusal does not name the key service: $(grep -i vault "$work/b.log" | tail -2)"
  [ "$(slots)" = kms,recovery ] || fail "the slots changed: $(slots)"
  grep -q "opened with its" "$work/b.log" && fail "it opened with another slot"
  echo "    running: signed in, a workspace made; a new start: $(grep -o 'no slot opens the vault.*' "$work/b.log" | head -1 | cut -c1-150)"

  step "5. OpenBao comes back, unsealed: the next start opens"
  bao_up
  bao_unseal
  [ "$(b read -field=latest_version transit/keys/ferrum)" = 2 ] || fail "the key did not survive the restart"
  opens c "a start with OpenBao back"
  stop_served
else
  echo "==> 4-5. skipped in dev mode: a development server keeps nothing across a stop (run it as 'file' for them)"
fi

step "6. the token's file replaced with a wrong one is refused as a refusal; with the right one it opens, nothing else restarted"
BAO_PID_BEFORE=$BAO_PID SERVER_PID_BEFORE=$SERVER_PID
printf '%s\n' "not-a-token" >"$TOKEN_FILE"
refused d "a start with a wrong token"
grep -q "the kms slot: the key-management service refused transit/decrypt/ferrum (403 Forbidden: permission denied). This is a refusal, not an outage" "$work/d.log" ||
  fail "the sentence for a wrong token: $(grep -o 'no slot opens.*' "$work/d.log" | head -1)"
[ "$(grep -c "trying again" "$work/d.log" || true)" = 0 ] || fail "a refusal was tried again"
printf '%s\n' "$GOOD_TOKEN" >"$TOKEN_FILE"
opens e "a start with the right token"
stop_served
[ "$BAO_PID" = "$BAO_PID_BEFORE" ] && kill -0 "$BAO_PID" && kill -0 "$SERVER_PID" && [ "$SERVER_PID" = "$SERVER_PID_BEFORE" ] || fail "something was restarted"
echo "    refused: $(grep -o 'refused transit/decrypt/ferrum.*not an outage' "$work/d.log" | head -1 | cut -c1-140); the right token: opens; OpenBao ($BAO_PID) and the first server ($SERVER_PID) are the same processes"

step "7. an AppRole instead of the token: the secret id in a file, wrong or right"
rm -f "$TOKEN_FILE"
ENVIRONMENT+=(FERRUM_KMS_APPROLE_ROLE_ID="$ROLE_ID")
printf '%s\n' "not-the-secret-id" >"$SECRET_ID_FILE"
refused f "a start with a wrong secret id"
grep -q "the kms slot: the key-management service refused auth/approle/login (400 Bad Request: invalid role or secret ID). This is a refusal, not an outage" "$work/f.log" ||
  fail "the sentence for a wrong secret id: $(grep -o 'no slot opens.*' "$work/f.log" | head -1)"
logins=$(audited auth/approle/login)
cp "$work/good.secret-id" "$SECRET_ID_FILE"
opens g "a start with the right secret id"
stop_served
[ "$(audited auth/approle/login)" -gt "$logins" ] || fail "OpenBao saw no AppRole login for the start"
rm -f "$SECRET_ID_FILE"
refused h "a start with no secret id file"
grep -q "secret id file .*kms.secret-id cannot be read" "$work/h.log" || fail "a missing secret id file is not said: $(grep -o 'no slot opens.*' "$work/h.log" | head -1)"
cp "$work/good.secret-id" "$SECRET_ID_FILE"
echo "    a wrong secret id: refused (400 invalid role or secret ID); the right one: opens, the login in OpenBao's audit log; no file: said"

step "8. the policy narrowed so that decrypt is denied: refused, said plainly; restored: opens"
policy "$POLICY_ENCRYPT_ONLY"
refused i "a start with decrypt denied"
grep -q "the kms slot: the key-management service refused transit/decrypt/ferrum (403 Forbidden: permission denied). This is a refusal, not an outage" "$work/i.log" ||
  fail "the sentence for a denied policy: $(grep -o 'no slot opens.*' "$work/i.log" | head -1)"
policy "$POLICY_RW"
opens j "a start with the policy restored"
stop_served
echo "    decrypt denied: refused as a refusal; restored: opens"

step "9. a weaker slot is never used silently: the credential slot beside the kms slot"
task vault_add_slot kind:credential >"$work/credential.log" 2>&1 || fail "the credential slot: $(tail -3 "$work/credential.log")"
[ "$(slots)" = credential,kms,recovery ] || fail "the slots: $(slots)"
line=$(doctor_line vault.root)
[[ "$line" == *"credential slot beside its kms slot"* ]] || fail "the doctor on a credential slot beside kms: $line"
if [ "$MODE" = file ]; then
  bao_stop
  opens k "a start with the key service away and a credential slot"
  grep -q "the vault's kms slot did not open (the key-management service cannot be reached.*); it opened with its credential slot" "$work/k.log" ||
    fail "the log does not say the kms slot did not open: $(grep -i "slot" "$work/k.log" | head -3)"
  stop_served
  echo "    OpenBao away: the vault opened with the credential slot, and the log says the kms slot did not and why"
fi
echo "    the doctor: ${line:0:150}"
echo "OK: the vault's root key is OpenBao's, and every way it can fail is said"
