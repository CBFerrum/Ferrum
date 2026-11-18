#!/usr/bin/env bash
# The example database chart (reference/kubernetes/database): it starts empty by default, or from another server or a backup,
# and refuses what a mode lacks; with the pooler it renders PgBouncer in front of the primary, and nothing without it. Needs
# helm and python3 with pyyaml; no cluster, no network. Run from the repo root: bash reference/kubernetes/database/test.sh
set -euo pipefail
cd "$(dirname "$0")/../../.."
chart=reference/kubernetes/database
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# What ships as a placeholder is a site's own to set (the chart refuses it otherwise).
tpl() { helm template ferrum "$chart" --namespace ferrum --set 'database.backup.destinationPath=s3://acme-backups/ferrum' "$@"; }
refuses() {
  local want=$1; shift
  if err=$(tpl "$@" 2>&1); then echo "rendered, but should have refused: $want"; exit 1; fi
  grep -qF "$want" <<<"$err" || { echo "refused for another reason than \"$want\": $err"; exit 1; }
}

echo "lint"
helm lint "$chart" --set 'database.backup.destinationPath=s3://acme-backups/ferrum'
echo "the placeholder fails"
if err=$(helm template ferrum "$chart" 2>&1); then echo "rendered with its placeholder"; exit 1; fi
grep -q 'database.backup.destinationPath' <<<"$err" || { echo "unexpected error: $err"; exit 1; }

echo "the database starts empty by default, or from another server or a backup, and refuses what a mode lacks"
src=(--set database.bootstrap.source.host=old.example.com --set database.bootstrap.source.passwordSecret.name=old-db)
for mode in import copy recovery; do
  extra=()
  case $mode in
    import) extra=("${src[@]}" --set database.bootstrap.source.user=postgres) ;;
    copy) extra=("${src[@]}" --set database.bootstrap.source.user=streaming_replica) ;;
    recovery) extra=(--set database.backup.serverName=ferrum-db-2 --set database.bootstrap.recovery.targetTime="2026-11-14 10:00:00+00") ;;
  esac
  tpl --set database.bootstrap.mode=$mode "${extra[@]}" > "$tmp/db-$mode.yaml"
done
tpl > "$tmp/db-empty.yaml"
python3 - "$tmp" <<'PY2'
import sys, yaml
def cluster(mode):
    docs = [d for d in yaml.safe_load_all(open(f"{sys.argv[1]}/db-{mode}.yaml")) if d]
    return next(d for d in docs if d["kind"] == "Cluster")["spec"]
assert cluster("empty")["bootstrap"] == {"initdb": {"database": "ferrum", "owner": "ferrum"}}
assert "externalClusters" not in cluster("empty")
c = cluster("import")
assert c["bootstrap"]["initdb"]["import"] == {"type": "microservice", "databases": ["ferrum"], "source": {"externalCluster": "origin"},
                                              "pgDumpExtraOptions": ["--enable-row-security"]}, c
assert c["externalClusters"][0]["connectionParameters"]["host"] == "old.example.com"
# The source's own role reads every workspace's rows: through the policies, as the system's own session.
assert c["externalClusters"][0]["connectionParameters"]["options"] == "-c ferrum.system=on -c ferrum.vault=on", c
assert "options" not in cluster("copy")["externalClusters"][0]["connectionParameters"]
assert c["externalClusters"][0]["password"] == {"name": "old-db", "key": "password"}
c = cluster("copy")
assert c["bootstrap"] == {"pg_basebackup": {"source": "origin", "database": "ferrum", "owner": "ferrum"}}, c
assert c["externalClusters"][0]["connectionParameters"]["user"] == "streaming_replica"
c = cluster("recovery")
assert c["bootstrap"]["recovery"]["recoveryTarget"] == {"targetTime": "2026-11-14 10:00:00+00"}, c
assert c["externalClusters"][0]["plugin"]["parameters"] == {"barmanObjectName": "ferrum-backups", "serverName": "ferrum-db"}
# The recovered cluster writes its own backups under its new name, never over the ones it reads.
assert c["plugins"][0]["parameters"] == {"barmanObjectName": "ferrum-backups", "serverName": "ferrum-db-2"}, c["plugins"]
assert "serverName" not in cluster("empty")["plugins"][0]["parameters"]
PY2
refuses "set database.bootstrap.source.host" --set database.bootstrap.mode=import
refuses "the role the server runs as there" --set database.bootstrap.mode=import --set database.bootstrap.source.host=h
refuses "REPLICATION and LOGIN" --set database.bootstrap.mode=copy --set database.bootstrap.source.host=h
refuses "source.passwordSecret.name" --set database.bootstrap.mode=copy --set database.bootstrap.source.host=h --set database.bootstrap.source.user=r
refuses "set database.backup.serverName to a new name" --set database.bootstrap.mode=recovery
refuses 'not "restore"' --set database.bootstrap.mode=restore
# Reading another cluster's backups needs no new name.
tpl --set database.bootstrap.mode=recovery --set database.bootstrap.recovery.serverName=old-db > /dev/null

echo "the role's limit and the backups' reach are what the server's values are told"
python3 - "$tmp/db-empty.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
c = next(d for d in docs if d["kind"] == "Cluster")["spec"]
role = c["managed"]["roles"][0]
assert role["name"] == "ferrum" and role["connectionLimit"] == 390 <= int(c["postgresql"]["parameters"]["max_connections"]) - 20, role
assert next(d for d in docs if d["kind"] == "ObjectStore")["spec"]["retentionPolicy"] == "30d"
assert {d["kind"] for d in docs} == {"Cluster", "ScheduledBackup", "ObjectStore", "PodMonitor"}, {d["kind"] for d in docs}
PY2
refuses "is not days, weeks or months" --set database.backup.retention=forever

echo "with the pooler, PgBouncer stands in front of the primary in transaction mode, with prepared statements, and only the server's pods reach it"
tpl --set database.pooler.enabled=true > "$tmp/pooler.yaml"
python3 - "$tmp/pooler.yaml" <<'PY2'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
p = next(d for d in docs if d["kind"] == "Pooler")
assert p["spec"]["cluster"] == {"name": "ferrum-db"} and p["spec"]["type"] == "rw"
assert p["spec"]["pgbouncer"]["poolMode"] == "transaction"
assert int(p["spec"]["pgbouncer"]["parameters"]["max_prepared_statements"]) > 0
pol = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "ferrum-db-pooler")
servers = pol["spec"]["ingress"][0]["from"][0]["podSelector"]["matchExpressions"][0]["values"]
assert servers == ["ferrum"], servers
assert any(d["kind"] == "PodDisruptionBudget" and d["metadata"]["name"] == "ferrum-db-pooler" for d in docs)
assert any(d["kind"] == "PodMonitor" and d["metadata"]["name"] == "ferrum-db-pooler" for d in docs)
PY2
tpl --set database.pooler.enabled=true --set-json 'serverPods=["ferrum","ferrum-worker"]' | grep -q 'values: \["ferrum","ferrum-worker"\]'
refuses "maxPreparedStatements must be at least 1" --set database.pooler.enabled=true --set database.pooler.maxPreparedStatements=0
! tpl | grep -q 'kind: Pooler' || exit 1
echo "ok"
