#!/usr/bin/env bash
# The restore drill (docs/operations.md, "Backups of files"): a database brought back to one moment, and every file its
# rows name opened in object storage and compared with its length and SHA-256.
#
#   deploy/drill.sh <dump> <address of an empty database>   # a dump taken at that moment (deploy/dump.sh dump), restored
#   deploy/drill.sh --restored <address>                    # a database already restored there (CloudNativePG's
#                                                            # recovery to a targetTime, a managed service's point in time)
#
# The check is the server's own (`ferrum task check_files digest:yes`), run with the servers' settings and secrets (run
# it where they are: in the server's container, or with its environment) on the restored database alone. It writes
# nothing to the live database and never takes the store back, so the live installation's restore marker
# (`restore-marker/` in the store) stays where it is: a copy with the live database's identity reads it, finds itself
# behind and is held; a copy with another identity keeps a marker of its own. Files deleted after that moment are there
# while their deletion waits (`files_delete_after_days`, at least `backup_reach_days`); files written after it are in the
# store and named by nothing in the copy.
#
# Settings: FERRUM_BIN (the server binary; default `ferrum` on the PATH), and what deploy/dump.sh takes. Run it from
# where the server's configuration is (the image's /app, or server/ in a checkout).
set -euo pipefail
usage() { echo "usage: $0 <dump> <empty database address> | $0 --restored <database address>" >&2; exit 2; }
[ $# -eq 2 ] || usage
bin=${FERRUM_BIN:-ferrum}
if [ "$1" = --restored ]; then
  url=$2
else
  "$(dirname "$0")/dump.sh" restore "$2" "$1"
  url=$2
fi
echo "checking every file the restored database names"
DATABASE_URL=$url "$bin" task check_files digest:yes
