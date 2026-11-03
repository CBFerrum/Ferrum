#!/usr/bin/env bash
# Changes how many database instances there are, or how large their volumes are (deploy/k8s/database.yaml), only
# when that is safe now:
#   * every instance ready, and every standby streaming within MAX_LAG_BYTES of the primary;
#   * at least minSyncReplicas + 1 instances left, so that writes keep a synchronous standby;
#   * volumes that only grow, on a storage class that can grow them.
# Otherwise it says why, changes nothing and exits 3. With --wait SECONDS it waits that long for replication to be
# healthy first. More instances are more copies to read from (ferrum-db-ro) and to fail over to: every write still
# goes to the one primary (ferrum-db-rw), so they are never more room for writes.
#
#   deploy/scale-database.sh --instances 4 [--wait 600]
#   deploy/scale-database.sh --storage 200Gi [--wait 600]
set -euo pipefail

NS=${NS:-ferrum}
CLUSTER=${CLUSTER:-ferrum-db}
MAX_LAG_BYTES=${MAX_LAG_BYTES:-16777216}
instances="" storage="" wait=0
while [ $# -gt 0 ]; do
  case "$1" in
    --instances) instances=${2:-}; shift 2 ;;
    --storage) storage=${2:-}; shift 2 ;;
    --wait) wait=${2:-}; shift 2 ;;
    *) echo "usage: $0 --instances N | --storage SIZE [--wait SECONDS]" >&2; exit 2 ;;
  esac
done
if { [ -z "$instances" ] && [ -z "$storage" ]; } || { [ -n "$instances" ] && [ -n "$storage" ]; }; then
  echo "say --instances or --storage, one of them" >&2
  exit 2
fi

k() { kubectl -n "$NS" "$@"; }
field() { k get cluster "$CLUSTER" -o jsonpath="$1"; }
refuse() { echo "not changed: $*" >&2; exit 3; }
bytes() {
  python3 -c 'import re, sys
m = re.fullmatch(r"(\d+)(Ki|Mi|Gi|Ti)?", sys.argv[1])
sys.exit(f"{sys.argv[1]}: a size such as 200Gi") if not m else print(int(m.group(1)) * {None: 1, "Ki": 1 << 10, "Mi": 1 << 20, "Gi": 1 << 30, "Ti": 1 << 40}[m.group(2)])' "$1"
}

# What does not change with time is refused at once.
if [ -n "$instances" ]; then
  [[ "$instances" =~ ^[0-9]+$ ]] || refuse "--instances takes a number"
  floor=$(($(field '{.spec.minSyncReplicas}') + 1))
  [ "$instances" -ge "$floor" ] || refuse "fewer than $floor instances would leave writes without a synchronous standby (minSyncReplicas)"
else
  [[ "$storage" =~ ^[0-9]+(Ki|Mi|Gi|Ti)?$ ]] || refuse "--storage takes a size such as 200Gi"
  now=$(field '{.spec.storage.size}')
  [ "$(bytes "$storage")" -gt "$(bytes "$now")" ] || refuse "volumes only grow, and they are $now now"
  class=$(field '{.spec.storage.storageClass}')
  [ -n "$class" ] || class=$(kubectl get storageclass -o jsonpath='{.items[?(@.metadata.annotations.storageclass\.kubernetes\.io/is-default-class=="true")].metadata.name}')
  [ "$(kubectl get storageclass "$class" -o jsonpath='{.allowVolumeExpansion}')" = true ] || refuse "the storage class $class cannot grow a volume"
fi

# What replication is doing now. Empty when it is healthy.
unsafe() {
  local want ready primary line streaming lag
  want=$(field '{.spec.instances}')
  ready=$(field '{.status.readyInstances}')
  [ "${ready:-0}" = "$want" ] || { echo "$ready of $want instances are ready"; return 0; }
  primary=$(field '{.status.currentPrimary}')
  line=$(k exec "$primary" -c postgres -- psql -tA -F ' ' -c \
    "select count(*), coalesce(max(pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn)), 0)::bigint from pg_stat_replication where state = 'streaming'" 2>/dev/null) ||
    { echo "the primary $primary does not answer"; return 0; }
  read -r streaming lag <<<"$line"
  [ "$streaming" = $((want - 1)) ] || { echo "$streaming of $((want - 1)) standbys are streaming"; return 0; }
  [ "$lag" -le "$MAX_LAG_BYTES" ] || echo "a standby is $lag bytes behind the primary"
}

deadline=$(($(date +%s) + wait))
while why=$(unsafe) && [ -n "$why" ]; do
  [ "$(date +%s)" -lt "$deadline" ] || refuse "replication is not healthy: $why"
  sleep 5
done

if [ -n "$instances" ]; then
  k patch cluster "$CLUSTER" --type merge -p "{\"spec\":{\"instances\":$instances}}" >/dev/null
  done_by=$(($(date +%s) + 900))
  until [ "$(field '{.status.readyInstances}')" = "$instances" ] && [ -z "$(unsafe)" ]; do
    [ "$(date +%s)" -lt "$done_by" ] || { echo "asked for $instances instances, and after 15 minutes: $(unsafe)" >&2; exit 1; }
    sleep 5
  done
else
  k patch cluster "$CLUSTER" --type merge -p "{\"spec\":{\"storage\":{\"size\":\"$storage\"}}}" >/dev/null
fi
echo "$CLUSTER: $(field '{.status.readyInstances}') instances ready, volumes of $(field '{.spec.storage.size}')"
