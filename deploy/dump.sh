#!/usr/bin/env bash
# A whole Ferrum database, dumped and restored as the server's own role: no superuser, so on any PostgreSQL
# (docs/operations.md, "Backups and restore" and "Any PostgreSQL").
#
#   deploy/dump.sh dump <address> <file>      # every row of every workspace, in pg_dump's custom format
#   deploy/dump.sh restore <address> <file>   # into an empty database the role owns, all or nothing
#
# <address> is PostgreSQL itself (the direct address, never a pooler in transaction mode: pg_dump needs one session,
# and PgBouncer refuses the session setting below), as the role the server runs as. Row-level security hides every
# workspace's rows from that role unless the session says it is the system's own (`ferrum.system`; `ferrum.vault`
# for the vault's tables), which this sets for pg_dump's session alone, with --enable-row-security so that pg_dump
# reads through the policies instead of refusing. Without the setting a dump would hold empty tables and say nothing.
#
# The restore leaves out what the role cannot make and the database already has: a schema that is there already (an
# administrator's `extensions`), and comments on extensions (only their owner may write them). Extensions themselves
# are made if missing (btree_gist and citext are trusted: the database's owner may), and kept if present.
# Afterwards the server holds what reaches outside until a person releases it (`ferrum task release_restore`), as
# after any restore; run `ferrum task reapply_erasures` first ("After restoring a backup").
#
# Settings: PG_BIN (where pg_dump, pg_restore and psql are; default: the PATH's).
set -euo pipefail
usage() { echo "usage: $0 dump|restore <address> <file>" >&2; exit 2; }
[ $# -eq 3 ] || usage
cmd=$1 url=$2 file=$3
bin() { if [ -n "${PG_BIN:-}" ]; then echo "$PG_BIN/$1"; else command -v "$1" || { echo "no $1 (set PG_BIN)" >&2; exit 2; }; fi; }
psql=$(bin psql)
as_system=(-c ferrum.system=on -c ferrum.vault=on)

case "$cmd" in
  dump)
    # The setting must reach the session, or the dump would be empty without a word.
    said=$(PGOPTIONS="${as_system[*]}" "$psql" "$url" -X -v ON_ERROR_STOP=1 -Atqc "select current_setting('ferrum.system', true)")
    [ "$said" = on ] || { echo "refused: the session did not take ferrum.system (a pooler in between?): give PostgreSQL's own address" >&2; exit 1; }
    # pg_dump reads each partition by its own name: one without its table's policies reads as empty, and its rows would be
    # left out without a word.
    bare=$("$psql" "$url" -X -v ON_ERROR_STOP=1 -Atqc "select string_agg(p, ', ') from ferrum_partitions_without_policies() p")
    [ -z "$bare" ] || { echo "refused: these partitions lack their table's policies, and their rows would be left out: $bare" >&2; exit 1; }
    PGOPTIONS="${as_system[*]}" "$(bin pg_dump)" --enable-row-security --format=custom --file="$file" "$url"
    echo "dumped to $file"
    ;;
  restore)
    [ -f "$file" ] || { echo "no file $file" >&2; exit 2; }
    tables=$("$psql" "$url" -X -v ON_ERROR_STOP=1 -Atqc "select count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace \
      where c.relkind in ('r', 'p') and n.nspname not in ('pg_catalog', 'information_schema') and n.nspname !~ '^pg_toast' \
      and not exists (select 1 from pg_depend d where d.objid = c.oid and d.deptype = 'e')")
    [ "$tables" = 0 ] || { echo "refused: restore into an empty database; this one has $tables tables" >&2; exit 1; }
    existing=$("$psql" "$url" -X -v ON_ERROR_STOP=1 -Atqc "select nspname from pg_namespace where nspname not in ('pg_catalog', 'information_schema') and nspname !~ '^pg_'")
    list=$(mktemp "${TMPDIR:-/tmp}/ferrum-restore.XXXXXX")
    trap 'rm -f "$list"' EXIT
    "$(bin pg_restore)" --list "$file" | while IFS= read -r line; do
      case "$line" in *" COMMENT - EXTENSION "*) continue ;; esac
      if [[ "$line" =~ \ SCHEMA\ -\ ([^ ]+)\  ]] && grep -qxF "${BASH_REMATCH[1]}" <<<"$existing"; then continue; fi
      printf '%s\n' "$line"
    done >"$list"
    "$(bin pg_restore)" --no-owner --no-privileges --exit-on-error --single-transaction --use-list="$list" --dbname="$url" "$file"
    echo "restored into the database; start the server on it, run ferrum task reapply_erasures, then ferrum task release_restore"
    ;;
  *) usage ;;
esac
