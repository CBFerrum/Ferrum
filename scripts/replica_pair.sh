#!/usr/bin/env bash
# A private PostgreSQL primary and its streaming standby on this machine, for trying the read address
# (FERRUM_DATABASE_READ_URL, docs/operations.md "Read address") against a real replica: crates/kernel/tests/replica_pair_it.rs,
# or a server started with DATABASE_URL on the primary and FERRUM_DATABASE_READ_URL on the standby.
#
#   scripts/replica_pair.sh up <dir>              # makes <dir>, a primary and its standby made with pg_basebackup
#   . <dir>/env                                   # the addresses, for the test
#   scripts/replica_pair.sh stop-standby <dir>    # stops the standby (fast: its sessions are ended), and
#   scripts/replica_pair.sh start-standby <dir>   # starts it again; the test does both
#   scripts/replica_pair.sh down <dir>            # stops both and deletes <dir>
#
# Nothing else is touched. `up` makes <dir> itself (it must not exist) and leaves a mark in it; every other command
# refuses a directory without that mark, and only ever runs PostgreSQL on <dir>/primary and <dir>/standby. Both listen
# on 127.0.0.1 alone, each on its own port, with no Unix socket (a path under a long directory would not fit one);
# the machine's own PostgreSQL is never used. The test role `ferrum_pair` is no superuser (superusers pass row-level
# security); it owns the database `ferrum_pair_test` and may pause and resume the standby's replay and promote it.
# The standby cancels a query that holds up its replay after `max_standby_streaming_delay` = 1s, and the primary runs
# no autovacuum, so that a recovery conflict happens when a test makes one and at no other time.
#
# Settings: PG_BIN (PostgreSQL 17's programs; default /opt/homebrew/opt/postgresql@17/bin when it is there, else the
# PATH's), REPLICA_PAIR_PORT (the primary's port, default 5441; the standby takes the next one).
set -euo pipefail
# On macOS the server refuses to start without a valid locale in the environment ("postmaster became multithreaded").
export LC_ALL=C
MARK=.ferrum-replica-pair
usage() { echo "usage: $0 up|down|stop-standby|start-standby <dir>" >&2; exit 2; }
[ $# -eq 2 ] || usage
cmd=$1 dir=$2
if [ -z "${PG_BIN:-}" ]; then
  if [ -x /opt/homebrew/opt/postgresql@17/bin/pg_ctl ]; then PG_BIN=/opt/homebrew/opt/postgresql@17/bin; else PG_BIN=$(dirname "$(command -v pg_ctl)"); fi
fi
for tool in initdb pg_ctl pg_basebackup psql; do [ -x "$PG_BIN/$tool" ] || { echo "no $tool in $PG_BIN (set PG_BIN)" >&2; exit 2; }; done

# Only a directory `up` made: it holds the mark, and the data directories are its own two.
ours() {
  [ -d "$dir" ] && [ -f "$dir/$MARK" ] || { echo "refused: $dir was not made by $0 up (no $MARK in it)" >&2; exit 2; }
  dir=$(cd "$dir" && pwd)
}
admin() { PGPASSFILE="$dir/pgpass" "$PG_BIN/psql" "host=127.0.0.1 port=$1 user=ferrum_pair_admin dbname=${2:-postgres}" -v ON_ERROR_STOP=1 -Atqc "$3"; }
running() { [ -f "$dir/$1/postmaster.pid" ] && "$PG_BIN/pg_ctl" -D "$dir/$1" status >/dev/null 2>&1; }
streaming() {
  for _ in $(seq 1 60); do
    [ "$(admin "$standby" postgres "select count(*) from pg_stat_wal_receiver where status = 'streaming'" 2>/dev/null)" = 1 ] && return 0
    sleep 0.5
  done
  echo "the standby is not streaming from the primary; see $dir/standby.log" >&2
  return 1
}

case "$cmd" in
  up)
    [ ! -e "$dir" ] || { echo "refused: $dir exists; $0 up makes it (run $0 down $dir first, if it is a pair's)" >&2; exit 2; }
    primary=${REPLICA_PAIR_PORT:-5441}
    standby=$((primary + 1))
    for port in "$primary" "$standby"; do
      if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "refused: something listens on 127.0.0.1:$port (set REPLICA_PAIR_PORT)" >&2; exit 2; fi
    done
    mkdir -p "$dir"
    dir=$(cd "$dir" && pwd)
    printf 'primary=%s\nstandby=%s\n' "$primary" "$standby" >"$dir/$MARK"
    pass=$(openssl rand -hex 16)
    printf '%s\n' "$pass" >"$dir/admin.pass"
    printf '127.0.0.1:*:*:ferrum_pair_admin:%s\n' "$pass" >"$dir/pgpass"
    chmod 600 "$dir/admin.pass" "$dir/pgpass"
    "$PG_BIN/initdb" -D "$dir/primary" -U ferrum_pair_admin --pwfile="$dir/admin.pass" --auth=scram-sha-256 -E UTF8 --no-locale >"$dir/initdb.log"
    cat >>"$dir/primary/postgresql.conf" <<EOF
# scripts/replica_pair.sh
listen_addresses = '127.0.0.1'
port = $primary
unix_socket_directories = ''
max_connections = 60
shared_buffers = 32MB
wal_level = replica
max_wal_senders = 4
wal_keep_size = 256MB
autovacuum = off
# A zone other than UTC, as a server's own default may be: what the server reads must not depend on it.
timezone = 'Asia/Kolkata'
EOF
    "$PG_BIN/pg_ctl" -D "$dir/primary" -l "$dir/primary.log" -w start >/dev/null
    admin "$primary" postgres "create role ferrum_pair login password 'ferrum_pair'"
    admin "$primary" postgres "create database ferrum_pair_test owner ferrum_pair"
    # In the test's database: a function's grants are kept per database.
    admin "$primary" ferrum_pair_test "grant execute on function pg_wal_replay_pause(), pg_wal_replay_resume(), pg_promote(boolean, integer) to ferrum_pair"
    # The standby, from a base backup of the primary that writes its own standby.signal and primary_conninfo.
    PGPASSFILE="$dir/pgpass" "$PG_BIN/pg_basebackup" -h 127.0.0.1 -p "$primary" -U ferrum_pair_admin -D "$dir/standby" -R -X stream -c fast
    cat >>"$dir/standby/postgresql.conf" <<EOF
# scripts/replica_pair.sh: the standby
port = $standby
hot_standby = on
max_standby_streaming_delay = 1s
EOF
    "$PG_BIN/pg_ctl" -D "$dir/standby" -l "$dir/standby.log" -w start >/dev/null
    streaming
    url="postgres://ferrum_pair:ferrum_pair@127.0.0.1"
    cat >"$dir/env" <<EOF
export FERRUM_TEST_REPLICA_PRIMARY_URL=$url:$primary/ferrum_pair_test
export FERRUM_TEST_REPLICA_URL=$url:$standby/ferrum_pair_test
export FERRUM_TEST_REPLICA_PAIR=$dir
EOF
    echo "the primary listens on 127.0.0.1:$primary and its standby on 127.0.0.1:$standby, streaming:"
    cat "$dir/env"
    ;;
  stop-standby)
    ours
    running standby && "$PG_BIN/pg_ctl" -D "$dir/standby" -m fast -w stop >/dev/null
    echo "the standby is stopped"
    ;;
  start-standby)
    ours
    standby=$(sed -n 's/^standby=//p' "$dir/$MARK")
    running standby || "$PG_BIN/pg_ctl" -D "$dir/standby" -l "$dir/standby.log" -w start >/dev/null
    # A standby promoted since is a primary of its own: started, it streams from nothing.
    if [ ! -f "$dir/standby/standby.signal" ]; then echo "the standby was promoted: it runs as a primary of its own"; exit 0; fi
    streaming
    echo "the standby streams from the primary again"
    ;;
  down)
    ours
    for side in standby primary; do
      if running "$side"; then "$PG_BIN/pg_ctl" -D "$dir/$side" -m fast -w stop >/dev/null; fi
    done
    rm -rf "$dir"
    echo "the pair is stopped and $dir deleted"
    ;;
  *) usage ;;
esac
