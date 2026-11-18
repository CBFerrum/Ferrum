# The single-host reference stack

**An example you own, not the product.** The product (`deploy/docker-compose.yml`) takes the address of a PostgreSQL and a
bucket of an S3-compatible store and operates neither. This stack runs both beside it so that one machine, one command and a
backup you can restore give you a working installation. What runs here is yours from then on.

## What it gives

* the two addresses the product takes: `DATABASE_URL` (a PostgreSQL 17 in the `db` service, the ordinary role `ferrum`, made by
  `postgres-init/`) and the bucket `ferrum` on the `store` service (SeaweedFS, one process, its S3 port on Compose's network
  alone), made once by the one-shot `store-setup` service (`ferrum task bucket_set_up`: versioning, lifecycle rules, encryption);
* a backup: the `backup` service runs `deploy/dump.sh dump` once a day into the `backups` volume and deletes the dumps older than
  `BACKUP_KEEP_DAYS` (30). That number is also what the product is told its backups reach, so a deleted file waits as long
  (`FERRUM_BACKUP_REACH_DAYS`, `FERRUM_FILES_DELETE_AFTER_DAYS`);
* one command: `FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:<commit> reference/single-host/up.sh` writes `reference/single-host/.env`
  once (the database's two passwords and the store's key made with `openssl`, kept) and runs
  `docker compose -f deploy/docker-compose.yml -f reference/single-host/compose.yml --env-file reference/single-host/.env up -d`.
  The product's file comes first so that this stack's values win the merge (Compose's `include` cannot add `depends_on` to a
  service it includes). The other Compose files (`deploy/mail.yml`, `meet.yml`, `editors.yml`, `live.yml`,
  `metrics-engine.yml`, `temporal.yml`) go after these two as in `docs/operations.md`; `temporal.yml` finds this stack's database
  through `TEMPORAL_PG_HOST`, `_USER` and `_PASSWORD` in the `.env`.

## What you are responsible for

* **Copying the dumps off the host.** The `backups` volume is on the same disk as the database. A backup that dies with the
  machine is not one: copy `ferrum-*.dump` somewhere else on a schedule (the volume's path: `docker volume inspect
  ferrum_backups`; or `docker compose cp backup:/backups/<file> .`).
* **Watching the job.** The doctor cannot see a dump (it says "cannot tell" for `database.backups`), and a failing dump is in
  `docker compose logs backup` and nowhere else.
* **The store's volume and key.** Files, journals and the restore marker are in the `store` volume, with the key that encrypts
  what the store keeps and its own copy of the access key. Versioning inside the same volume is not durability: archive the
  volume (below) as carefully as the dump, and keep `.env`, which holds the key.
* **PostgreSQL's upgrades and the disk.** `postgres:17` is pinned to a major version; moving to another is a dump and a restore
  (`docs/operations.md`, "Backups and restore"). Nothing here watches the disk, rotates logs, or puts TLS in front of the
  server: that is a reverse proxy of yours (`PUBLIC_URL`, `FERRUM_PROXY_HOPS`).
* **Rotating the passwords and the key.** Changing the database role's password needs `ferrum task vault_rewrap`
  (`docs/operations.md`, "The vault"); changing the store's key means changing it in `.env` and recreating `store`,
  `store-setup` and `ferrum` together.

If you would rather run neither, give the product your own addresses (`deploy/.env.example`) and use none of this.

## Backing up and restoring

Every command is run from the repository's root, with the stack's two files; call it
`C="docker compose -f deploy/docker-compose.yml -f reference/single-host/compose.yml --env-file reference/single-host/.env"`.
**Not run here: there is no Docker daemon on the machine this was written on**; `reference/single-host/smoke.sh` runs the
restore (a dump taken by the job's own command, the store's volume, files intact) where there is one.

The backup, the dump first, then the store's volume while the store is paused (uploads and downloads wait meanwhile; a large
store takes a while to archive). Set `FERRUM_FILES_DELETE_AFTER_DAYS` to at least the time between the two, so nothing the dump
names is deleted before the archive:

```bash
$C exec -T backup sh -c 'dump.sh dump "$DATABASE_URL" /tmp/now.dump >&2 && cat /tmp/now.dump' > ferrum-$(date +%F).dump
$C pause store
$C run --rm --no-deps -T --entrypoint tar store -C /data -czf - . > store-$(date +%F).tgz
$C unpause store
```

(The `backup` service's own dumps are in `/backups`: the first of these lines is the same command on demand.) Take the archive
after the database, so that the files include everything the dump refers to and every erasure journalled since
(`privacy-journal/`, which `ferrum task reapply_erasures` replays after a restore).

Onto a new host (the same `.env`), the database and the store's volume first, then the rest:

```bash
$C up -d db
$C exec db pg_isready -h 127.0.0.1                  # until it answers
$C exec -T db pg_restore -U postgres -d ferrum --clean --if-exists --exit-on-error < ferrum.dump
$C run --rm --no-deps -T --entrypoint tar store -C /data -xzf - < store.tgz
$C up -d
```

In place, into an empty database and an empty store volume (the project is named `ferrum`, so its volume is `ferrum_store`;
`pg_restore --clean` over the old database cannot drop a partitioned table's partitions one by one, and stops partway):

```bash
$C stop ferrum store
$C rm -f store
docker volume rm ferrum_store
$C exec -T db dropdb -U postgres --force ferrum
$C exec -T db createdb -U postgres -O ferrum ferrum
$C exec -T db pg_restore -U postgres -d ferrum --exit-on-error < ferrum.dump
$C run --rm --no-deps -T --entrypoint tar store -C /data -xzf - < store.tgz
$C up -d
```

The keys come back with the vault in the dump, and the store's own with its volume. Then, as after any restore,
`$C exec ferrum ferrum task reapply_erasures`, and `ferrum task release_restore` once checked (`docs/operations.md`, "After
restoring a backup"); `deploy/drill.sh` checks every file a row names. Test a restore at least quarterly.
