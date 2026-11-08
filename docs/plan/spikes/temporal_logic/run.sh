#!/bin/bash
# Spike L-01: runs every probe against a live Temporal server on PostgreSQL, including two restores out of
# step. Needs: a built spike (cargo build), the Temporal server and CLI from docs/plan/spike-l01-temporal.md,
# and these set:
#   SPIKE      the built binary            TEMPORAL_BIN   the temporal CLI
#   START_SRV  a script starting the server, logging where it likes; the server is stopped by its pid
#   TDB, TVDB  Temporal's two databases    FERRUM_SPIKE_DB the stand-in for Ferrum's records
set -euo pipefail
: "${SPIKE:?}" "${TEMPORAL_BIN:?}" "${START_SRV:?}" "${TDB:?}" "${TVDB:?}" "${FERRUM_SPIKE_DB:?}"
PG=postgres://ferrum:ferrum@localhost:5432
WORK=$(mktemp -d)
log() { printf '\n== %s\n' "$*"; }
healthy() { for _ in $(seq 1 60); do "$TEMPORAL_BIN" operator cluster health --address 127.0.0.1:7233 2>/dev/null | grep -q SERVING && return 0; sleep 1; done; echo "server never came up"; return 1; }
srv_pid() { pgrep -f 'temporal-server .* start' || true; }
stop_srv() { for p in $(srv_pid); do kill "$p"; done; for _ in $(seq 1 30); do [ -z "$(srv_pid)" ] && return 0; sleep 1; done; echo "server did not stop"; return 1; }
start_srv() { (nohup "$START_SRV" >/dev/null 2>&1 &); healthy; }
dump() { pg_dump -Fc "$PG/$1" -f "$WORK/$2.dump"; }
restore() { pg_restore --clean --if-exists --no-owner -d "$PG/$1" "$WORK/$2.dump" 2>/dev/null || true; }
rss() { ps -o rss= -p "$1" | awk '{printf "%.0f MB", $1/1024}'; }
wait_sent() { for _ in $(seq 1 60); do "$SPIKE" gate-ledger "$1" | grep -q "$1/a sent=1" && return 0; sleep 1; done; echo "effect a never ran"; return 1; }

"$SPIKE" setup
healthy
log "worker"
("$SPIKE" worker > "$WORK/worker.log" 2>&1 &)
for _ in $(seq 1 30); do grep -q polling "$WORK/worker.log" && break; sleep 1; done
worker_pid=$(pgrep -f "$SPIKE worker" | head -1)

for p in effect early-signal revoked race codec replay carry; do log "$p"; "$SPIKE" "$p"; done

log "crash: the worker is killed after the work is done, before it answers"
C="p0-$(date +%s)"
"$SPIKE" crash-start "$C"
for _ in $(seq 1 30); do "$SPIKE" gate-ledger "$C" | grep -q "$C/crash sent=1" && break; sleep 1; done
pkill -9 -f "$SPIKE worker" || true
("$SPIKE" worker >> "$WORK/worker.log" 2>&1 &)
"$SPIKE" crash-result "$C"

log "restore A: Temporal restored to before an effect Ferrum recorded"
G1="p8a-$(date +%s)"
"$SPIKE" gate-start "$G1"
stop_srv; dump "$TDB" t0; dump "$TVDB" tv0; start_srv
"$SPIKE" gate-signal "$G1" go; wait_sent "$G1"
stop_srv; restore "$TDB" t0; restore "$TVDB" tv0; start_srv
pkill -f "$SPIKE worker" || true; ("$SPIKE" worker >> "$WORK/worker.log" 2>&1 &); sleep 3
"$SPIKE" gate-signal "$G1" go
"$SPIKE" gate-signal "$G1" finish
"$SPIKE" gate-result "$G1"

log "restore B: Ferrum restored to before an effect it made; the restore runbook bumps the recovery epoch"
G2="p8b-$(date +%s)"
"$SPIKE" gate-start "$G2"
dump "${FERRUM_SPIKE_DB##*/}" f0
"$SPIKE" gate-signal "$G2" go; wait_sent "$G2"
restore "${FERRUM_SPIKE_DB##*/}" f0
"$SPIKE" epoch-bump
"$SPIKE" gate-signal "$G2" finish
"$SPIKE" gate-result "$G2"

log "footprint"
worker_pid=$(pgrep -f "$SPIKE worker" | head -1)
echo "server rss at rest: $(rss "$(srv_pid | head -1)"), worker rss: $(rss "$worker_pid")"
"$SPIKE" load 200
echo "server rss after 200 workflows: $(rss "$(srv_pid | head -1)"), worker rss: $(rss "$worker_pid")"
for d in "$TDB" "$TVDB"; do echo "$d: $(psql -At "$PG/$d" -c "select pg_size_pretty(pg_database_size(current_database()))")"; done
echo "spike binary: $(du -h "$SPIKE" | cut -f1)"
pkill -f "$SPIKE worker" || true
rm -rf "$WORK"
