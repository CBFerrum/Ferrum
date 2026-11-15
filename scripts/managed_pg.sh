#!/usr/bin/env bash
# A private PostgreSQL on this machine that behaves like a managed service (docs/operations.md, "Any PostgreSQL"):
# the application's role is no superuser, has no BYPASSRLS, cannot make databases or roles, owns its database and
# nothing else, holds no grant on session_replication_role, and finds btree_gist, citext and pg_trgm already created
# by an administrator (in a schema of their own, `extensions`, on the role's search path, as Supabase has them). It is
# reached through PgBouncer in transaction mode (the pooled address), with PostgreSQL itself beside it (the direct
# address), as a managed service's pooler and primary are.
#
#   scripts/managed_pg.sh up <dir>          # makes <dir>: the cluster, the role, its first database, PgBouncer
#   . <dir>/env                             # the addresses (pooled, direct, the administrator's)
#   scripts/managed_pg.sh db <dir> <name>   # one more database the role owns, with the extensions made for it
#   scripts/managed_pg.sh serve <dir>       # the server binary on a database of its own, through PgBouncer, then stopped
#   scripts/managed_pg.sh down <dir>        # stops both and deletes <dir>
#
# Nothing else is touched. `up` makes <dir> itself (it must not exist) and leaves a mark in it; every other command
# refuses a directory without that mark. PostgreSQL and PgBouncer listen on 127.0.0.1 alone, each on its own port,
# with no Unix socket; the machine's own PostgreSQL is never used. The administrator stands for the service's own
# (an RDS master user, Supabase's `postgres`): it is used to make the role and its databases, and by tests that set
# something up that only an administrator could.
#
# Settings: PG_BIN (PostgreSQL 17's programs; default /opt/homebrew/opt/postgresql@17/bin when it is there, else the
# PATH's), PGBOUNCER (default /opt/homebrew/bin/pgbouncer, else the PATH's), MANAGED_PG_PORT (PostgreSQL's port,
# default 5451; PgBouncer takes the next one), FERRUM_BIN (for serve: the server binary; default
# $CARGO_TARGET_DIR/debug/ferrum), MANAGED_SERVE_PORT (the server's port for serve, default 5397).
set -euo pipefail
# On macOS the server refuses to start without a valid locale in the environment ("postmaster became multithreaded").
export LC_ALL=C
MARK=.ferrum-managed-pg
ROLE=ferrum_app
usage() { echo "usage: $0 up|serve|down <dir>, or $0 db <dir> <name>" >&2; exit 2; }
[ $# -ge 2 ] || usage
cmd=$1 dir=$2
if [ -z "${PG_BIN:-}" ]; then
  if [ -x /opt/homebrew/opt/postgresql@17/bin/pg_ctl ]; then PG_BIN=/opt/homebrew/opt/postgresql@17/bin; else PG_BIN=$(dirname "$(command -v pg_ctl)"); fi
fi
if [ -z "${PGBOUNCER:-}" ]; then
  if [ -x /opt/homebrew/bin/pgbouncer ]; then PGBOUNCER=/opt/homebrew/bin/pgbouncer; else PGBOUNCER=$(command -v pgbouncer || true); fi
fi
for tool in initdb pg_ctl psql; do [ -x "$PG_BIN/$tool" ] || { echo "no $tool in $PG_BIN (set PG_BIN)" >&2; exit 2; }; done
[ -x "${PGBOUNCER:-}" ] || { echo "no pgbouncer (set PGBOUNCER)" >&2; exit 2; }

ours() {
  [ -d "$dir" ] && [ -f "$dir/$MARK" ] || { echo "refused: $dir was not made by $0 up (no $MARK in it)" >&2; exit 2; }
  dir=$(cd "$dir" && pwd)
  port=$(sed -n 's/^port=//p' "$dir/$MARK")
  bouncer=$(sed -n 's/^bouncer=//p' "$dir/$MARK")
}
admin() { PGPASSFILE="$dir/pgpass" "$PG_BIN/psql" "host=127.0.0.1 port=$port user=ferrum_managed_admin dbname=${1:-postgres}" -v ON_ERROR_STOP=1 -Atqc "$2"; }

# A database the role owns, with the extensions an administrator made in it.
database() {
  [[ "$1" =~ ^[a-z][a-z0-9_]*$ ]] || { echo "refused: a database's name is lower-case letters, digits and _" >&2; exit 2; }
  admin postgres "create database $1 owner $ROLE"
  admin "$1" "revoke all on database $1 from public; grant connect, temporary on database $1 to $ROLE"
  admin "$1" "create schema extensions; grant usage on schema extensions to $ROLE; \
    create extension btree_gist schema extensions; create extension citext schema extensions; create extension pg_trgm schema extensions"
  # PgBouncer is told of every database by name.
  printf '%s = host=127.0.0.1 port=%s dbname=%s\n' "$1" "$port" "$1" >>"$dir/databases.ini"
  if [ -f "$dir/pgbouncer.pid" ]; then kill -HUP "$(cat "$dir/pgbouncer.pid")"; sleep 0.5; fi
}

case "$cmd" in
  up)
    [ $# -eq 2 ] || usage
    [ ! -e "$dir" ] || { echo "refused: $dir exists; $0 up makes it" >&2; exit 2; }
    port=${MANAGED_PG_PORT:-5451}
    bouncer=$((port + 1))
    for p in "$port" "$bouncer"; do
      if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$p (set MANAGED_PG_PORT)" >&2; exit 2; fi
    done
    mkdir -p "$dir"
    dir=$(cd "$dir" && pwd)
    printf 'port=%s\nbouncer=%s\n' "$port" "$bouncer" >"$dir/$MARK"
    pass=$(openssl rand -hex 16)
    printf '%s\n' "$pass" >"$dir/admin.pass"
    printf '127.0.0.1:*:*:ferrum_managed_admin:%s\n' "$pass" >"$dir/pgpass"
    chmod 600 "$dir/admin.pass" "$dir/pgpass"
    "$PG_BIN/initdb" -D "$dir/data" -U ferrum_managed_admin --pwfile="$dir/admin.pass" --auth=scram-sha-256 -E UTF8 --no-locale >"$dir/initdb.log"
    # TLS as a service has it: a certificate authority of its own (ca.pem, which an operator mounts as
    # /run/ferrum/db-ca.pem) signed the server's certificate, for 127.0.0.1 and localhost; other-ca.pem signed nothing here.
    tls() { openssl "$@" >>"$dir/tls.log" 2>&1; }
    tls req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=ferrum-managed-ca -keyout "$dir/ca.key" -out "$dir/ca.pem"
    tls req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=ferrum-other-ca -keyout "$dir/other-ca.key" -out "$dir/other-ca.pem"
    tls req -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$dir/data/server.key" -out "$dir/server.csr"
    printf 'subjectAltName = IP:127.0.0.1, DNS:localhost\n' >"$dir/san.ext"
    tls x509 -req -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" -CAcreateserial -days 30 -extfile "$dir/san.ext" -out "$dir/data/server.crt"
    chmod 600 "$dir/data/server.key"
    cat >>"$dir/data/postgresql.conf" <<EOF
# scripts/managed_pg.sh
listen_addresses = '127.0.0.1'
port = $port
unix_socket_directories = ''
max_connections = 120
shared_buffers = 64MB
max_locks_per_transaction = 256
ssl = on
# A zone other than UTC, as a service's own default may be: what the server reads must not depend on it.
timezone = 'Asia/Kolkata'
EOF
    "$PG_BIN/pg_ctl" -D "$dir/data" -l "$dir/postgres.log" -w start >/dev/null
    app=$(openssl rand -hex 12)
    admin postgres "create role $ROLE login password '$app' nosuperuser nobypassrls nocreatedb nocreaterole noreplication connection limit 100"
    admin postgres "alter role $ROLE set search_path = \"\$user\", public, extensions"
    : >"$dir/databases.ini"
    database ferrum_managed
    # PgBouncer checks the role's password as PostgreSQL keeps it (SCRAM), and passes it on.
    secret=$(admin postgres "select rolpassword from pg_authid where rolname = '$ROLE'")
    printf '"%s" "%s"\n' "$ROLE" "$secret" >"$dir/userlist.txt"
    chmod 600 "$dir/userlist.txt"
    cat >"$dir/pgbouncer.ini" <<EOF
[databases]
%include $dir/databases.ini
[pgbouncer]
listen_addr = 127.0.0.1
listen_port = $bouncer
unix_socket_dir =
auth_type = scram-sha-256
auth_file = $dir/userlist.txt
pool_mode = transaction
max_prepared_statements = 200
default_pool_size = 30
max_client_conn = 400
server_reset_query =
ignore_startup_parameters = extra_float_digits
logfile = $dir/pgbouncer.log
pidfile = $dir/pgbouncer.pid
EOF
    "$PGBOUNCER" -d "$dir/pgbouncer.ini"
    for _ in $(seq 1 40); do (exec 3<>"/dev/tcp/127.0.0.1/$bouncer") 2>/dev/null && break; sleep 0.25; done
    cat >"$dir/env" <<EOF
export FERRUM_MANAGED_DIR=$dir
export FERRUM_MANAGED_URL=postgres://$ROLE:$app@127.0.0.1:$bouncer/ferrum_managed
export FERRUM_MANAGED_DIRECT_URL=postgres://$ROLE:$app@127.0.0.1:$port/ferrum_managed
export FERRUM_MANAGED_ADMIN_URL=postgres://ferrum_managed_admin:$pass@127.0.0.1:$port/postgres
export FERRUM_MANAGED_BASE=postgres://$ROLE:$app@127.0.0.1
export FERRUM_MANAGED_PORTS="$port $bouncer"
export FERRUM_TEST_TLS_URL=postgres://$ROLE:$app@127.0.0.1:$port/ferrum_managed
export FERRUM_TEST_TLS_CA=$dir/ca.pem
export FERRUM_TEST_TLS_OTHER_CA=$dir/other-ca.pem
EOF
    chmod 600 "$dir/env"
    echo "PostgreSQL listens on 127.0.0.1:$port (direct) and PgBouncer, in transaction mode, on 127.0.0.1:$bouncer (pooled): . $dir/env"
    ;;
  db)
    [ $# -eq 3 ] || usage
    ours
    database "$3"
    echo "database $3 made, owned by $ROLE, with the extensions"
    ;;
  serve)
    # The server binary on a database of its own, through the pooler with only a direct host, as on a managed
    # service: it starts and serves, the doctor says copies need no grant and the restore marker was compared, and
    # the marker is in its object storage (here a directory).
    [ $# -eq 2 ] || usage
    ours
    . "$dir/env"
    root=$(cd "$(dirname "$0")/.." && pwd)
    bin=$(cd "$root" && realpath "${FERRUM_BIN:-${CARGO_TARGET_DIR:-target}/debug/ferrum}")
    [ -x "$bin" ] || { echo "no server binary at $bin (cargo build -p ferrum-server --no-default-features --bin ferrum)" >&2; exit 2; }
    # The server reads its configuration from config/ where it runs.
    cd "$root/server"
    database ferrum_serve
    serve_port=${MANAGED_SERVE_PORT:-5397}
    export LOCO_ENV=development LOG_LEVEL=warn PORT=$serve_port BINDING=127.0.0.1 FERRUM_FILES_DIR=$dir/files \
      DATABASE_URL=${FERRUM_MANAGED_URL%/*}/ferrum_serve FERRUM_DATABASE_DIRECT_HOST=127.0.0.1:$port \
      JWT_SECRET="$(openssl rand -base64 48 | tr -d '\n')" FERRUM_SECRET_KEY="$(openssl rand -base64 32 | tr -d '\n')"
    "$bin" start >"$dir/serve.log" 2>&1 &
    pid=$!
    trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
    for _ in $(seq 1 120); do
      curl -fsS "http://127.0.0.1:$serve_port/api/ready" >/dev/null 2>&1 && break
      kill -0 "$pid" 2>/dev/null || { tail -30 "$dir/serve.log"; echo "the server stopped" >&2; exit 1; }
      sleep 1
    done
    curl -fsS "http://127.0.0.1:$serve_port/api/ready" >/dev/null || { tail -30 "$dir/serve.log"; echo "not ready in 120 s" >&2; exit 1; }
    "$bin" task doctor >"$dir/doctor.txt" 2>&1 || true
    for check in database.role database.copy database.replication_role restore.marker failover; do
      grep -qE "^ok +$check " "$dir/doctor.txt" || { cat "$dir/doctor.txt"; echo "the doctor does not say $check is fine" >&2; exit 1; }
    done
    ls "$dir/files/restore-marker/"*.json >/dev/null 2>&1 || { echo "no restore marker in the files" >&2; exit 1; }
    grep -E "^(ok|warning|BLOCKER|off) +(database|restore|failover)" "$dir/doctor.txt"
    echo "the server served through PgBouncer with only a direct host, and its doctor says so"
    ;;
  down)
    [ $# -eq 2 ] || usage
    ours
    if [ -f "$dir/pgbouncer.pid" ]; then kill "$(cat "$dir/pgbouncer.pid")" 2>/dev/null || true; sleep 0.5; fi
    if [ -f "$dir/data/postmaster.pid" ]; then "$PG_BIN/pg_ctl" -D "$dir/data" -m fast -w stop >/dev/null || true; fi
    rm -rf "$dir"
    echo "stopped, and $dir deleted"
    ;;
  *) usage ;;
esac
