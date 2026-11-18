# The Kubernetes reference stack

**An example you own, not the product.** The product chart (`deploy/helm/ferrum`) takes the address of a PostgreSQL and a bucket
of an S3-compatible store and operates neither. This stack runs both in the cluster, as an example, and installs the chart on
them with the two addresses passed through. It has not been run on a cluster yet (`deploy/kind/smoke.sh` and
`reference/kubernetes/smoke.sh` are written for it); `helm template` and the tests in this folder run without one.

## What it gives

* `database/`: a small Helm chart for **CloudNativePG** (operator 1.30.1 and its Barman Cloud plugin 0.15.1): a `Cluster` of three
  instances with synchronous replication and automatic failover, continuous WAL archiving and a daily base backup into object
  storage (`retention: 30d`), the role `ferrum` with a connection limit of 390, an optional PgBouncer `Pooler`, and the ways to
  start from another database (`database.bootstrap.mode`: `empty`, `import`, `copy`, `recovery`). `database/test.sh` renders
  each mode and the pooler.
* `store.yaml` and `../store/start.sh`: **SeaweedFS** in one StatefulSet behind the Service `ferrum-store` (port 8333), its
  access key from the Secret `ferrum-store-keys`, which `install.sh` makes once. It makes no bucket.
* `ferrum-values.yaml`: the product chart's values for these two: `database.urlSecret` is `ferrum-db-app` (key `uri`),
  `database.connectionLimit` 390 (the role's limit), `database.backupReachDays` 30 (the backups' retention), the network
  policy's peers for the database's and the store's pods, `storage.endpoint` `http://ferrum-store:8333`, and
  `autoscaling.maxReplicas` 8 (nine replicas, a rollout and the migrate Job do not fit 390).
* `install.sh`: the operator and its plugin, the namespace and the keys, the store, a bucket for the database's backups, the
  database, the product (`helm upgrade --install ferrum deploy/helm/ferrum -f ferrum-values.yaml -f <yours>`), and then the
  product's own bucket: `ferrum task bucket_set_up` (versioning, lifecycle rules, encryption), run in a server pod once it is
  running, because the server is not ready until the bucket exists.

```bash
reference/kubernetes/install.sh -f my-values.yaml [-d database-values.yaml]
```

`my-values.yaml` gives at least `image.tag` (or `image.digest`), `publicUrl`, `gateway.hostname` and `office.host`, which ship as
placeholders. A Gateway API implementation, cert-manager and metrics-server come first (`deploy/kind/install.sh` installs what CI
uses).

## What you are responsible for

* **The database.** CloudNativePG keeps it running; you decide its sizes (`database/values.yaml`), its upgrades (the operator's,
  and PostgreSQL's major version, which is a dump and a restore), and whether its backups reach what you need. The
  doctor cannot read when the last base backup ran (`database.backups` says "cannot tell"): watch `kubectl get backup` and the
  operator's metrics. The product is told how far back the backups reach (`database.backupReachDays`, 30): if you change the
  retention, change that too, so that deleted files wait as long.
* **The backups' bucket.** They go to the same SeaweedFS (`ferrum-backups`, made by `install.sh`): one volume holds both the files
  and the database's backups, which is no protection against losing that volume. Point `database.backup.destinationPath` and
  `endpointURL` at a store of another failure domain (`-d`).
* **The store.** One StatefulSet, one volume of 20 Gi, no replication: a lost volume is lost files. Versioning inside the same
  volume is not durability. Its key is the Secret `ferrum-store-keys`; the server and the backups use it too (`ferrum-secrets`,
  `ferrum-backup-s3`).
* **The cluster's own parts**: the gateway, certificates, the storage class, the nodes.

A database or a bucket of your own replaces any of this: give the chart your addresses (`docs/operations.md`, "First deployment
(Kubernetes)") and run none of it.

## The pooler

`database.pooler.enabled: true` (in `-d`) adds a PgBouncer in transaction mode with prepared statements. The server must then be
given the pooler's address and PostgreSQL's own host. CloudNativePG's Secret holds the primary's address, so make the pooled one
and name the direct host:

```bash
kubectl -n ferrum create secret generic ferrum-db-pooled \
  --from-literal=uri="$(kubectl -n ferrum get secret ferrum-db-app -o jsonpath='{.data.uri}' | base64 -d | sed 's/ferrum-db-rw/ferrum-db-pooler/')"
# product values:
#   database: {urlSecret: {name: ferrum-db-pooled}, directHost: ferrum-db-rw.ferrum.svc}
```

Then the chart counts only each replica's 2 direct connections against `connectionLimit`; the pooler's own pool
(`instances x poolSize`) is yours to fit in the role's limit.

## Backing up, restoring, changing the database

CloudNativePG archives WAL continuously through the Barman Cloud plugin and takes a base backup daily (`ScheduledBackup`),
keeping 30 days (the `ObjectStore`'s `retentionPolicy`). On-demand backup:
`kubectl -n ferrum cnpg backup ferrum-db --method plugin --plugin-name barman-cloud.cloudnative-pg.io` (or create a `Backup`
resource with `method: plugin`).

Point-in-time restore: create a new cluster recovering from the object store, shaped like `restore.yaml`
(`bootstrap.recovery.source` names an external cluster read through the plugin, `barmanObjectName: ferrum-backups`,
`serverName: ferrum-db`), adding `recoveryTarget: {targetTime: "2026-10-28 10:00:00+00"}`; verify it, then point the Secret
`ferrum-db-app` / `DATABASE_URL` at it and restart the API pods. `smoke.sh` restores a backup this way and checks the latest
writes are in it. After any restore, `ferrum task reapply_erasures` and `ferrum task release_restore` (`docs/operations.md`,
"After restoring a backup").

`scale-database.sh --instances N` or `--storage 200Gi` (`--wait 600` to wait for replication to recover first) changes the number of
instances or the size of their volumes only when that is safe, and otherwise says why and exits 3: an instance is not ready; a
standby is not streaming, or is more than `MAX_LAG_BYTES` (16 MiB) behind; fewer instances than `minSyncReplicas + 1` would be
left, so writes would have no synchronous standby; a volume would shrink, or the storage class cannot grow one. More instances are
more copies to read from (`ferrum-db-ro`, which `database.readHost` of the product's values can name) and to fail over to; every
write still goes to the one primary (`ferrum-db-rw`).

## Checking it

`reference/kubernetes/smoke.sh` (on a cluster with the stack installed): three instances; the primary killed, a standby promoted and
writes resumed within 90 s with nothing lost; growing and shrinking refused while a standby is fenced and done under writes
after; a backup restored into a new cluster that holds the latest writes. `deploy/kind/smoke.sh` and `deploy/kind/pools.sh` run
the product on this stack.
