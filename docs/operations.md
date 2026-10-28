# Operations runbook

## Components

Ferrum keeps no state on its own disks or in its memory that matters beyond a request: data,
documents, plugin modules and their key-value state live in PostgreSQL, files in object storage,
sessions are signed tokens, and caches (schemas, users, plugins) are invalidated across replicas
through PostgreSQL `NOTIFY`. Any replica can be killed or added at any time.

| Component | Role | Failure behaviour |
|---|---|---|
| API replicas (`ferrum start`) | Stateless HTTP API; each also runs the outbox dispatcher, webhook sender, metadata listener and maintenance loops | Any replica may die at any time; work is claimed with `SKIP LOCKED` / advisory locks and retried by the others |
| PostgreSQL 17 (CloudNativePG) | All state: documents, ledgers, audit, outbox, plugins | Synchronous replica + automatic failover; WAL archiving for point-in-time recovery |
| Object storage (S3 or compatible) | Attachments (`FERRUM_S3_BUCKET`, `FERRUM_S3_ENDPOINT`) | Required in production; checked with a write/delete probe at boot (logged, not fatal) |

## Configuration

Production reads `config/production.yaml`, which takes everything from the environment:

| Variable | Required | Meaning |
|---|---|---|
| `DATABASE_URL` | yes | `postgres://user:pass@host:5432/db` |
| `JWT_SECRET` | yes | Base64, ≥ 32 random bytes: `openssl rand -base64 48`. Rotating it signs everyone out |
| `FERRUM_BOOTSTRAP_TOKEN` | no | Enables `POST /api/tenants` with header `X-Ferrum-Bootstrap-Token`; empty disables it |
| `FERRUM_METRICS_TOKEN` | recommended | `/metrics` then requires `Authorization: Bearer <token>` |
| `FERRUM_S3_BUCKET`, `FERRUM_S3_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | multi-replica | Attachment storage on S3 (region `auto` if empty) |
| `FERRUM_S3_ENDPOINT` | no | Any S3-compatible store: MinIO, Cloudflare R2, Wasabi, Backblaze B2, DigitalOcean Spaces, GCS interoperability |
| `FERRUM_REQUIRE_OBJECT_STORAGE` | no | Production default `true`: refuse to start without object storage, so no replica keeps files locally. `false` only for single-server installs |
| `FERRUM_FILES_DIR` | no | Local attachment directory (default `/var/lib/ferrum/files`) |
| `FERRUM_DB_MAX_CONNECTIONS` | no | Pool size per replica (default 30) |
| `FERRUM_RUN_BACKGROUND` | no | `false` turns a replica into API-only |
| `PUBLIC_URL`, `PORT`, `BINDING`, `LOG_LEVEL` | no | Usual meanings; logs are JSON on stdout |
| `MAILER_*` | no | SMTP for notifications |

**Database role.** Ferrum must connect as an ordinary role that owns its database, never as a
superuser or a role with `BYPASSRLS`: those bypass PostgreSQL row-level security and with it tenant
isolation, so the server refuses to start with them. CloudNativePG's `ferrum` application user and
the Compose setup (`deploy/postgres-init`) are already set up this way. The role needs to create the
trusted extensions `btree_gist`, `citext` and `pg_trgm`, which a database owner can do.

Each replica opens up to `FERRUM_DB_MAX_CONNECTIONS` connections plus 4 for short side writes
(document numbering). Keep `replicas × (FERRUM_DB_MAX_CONNECTIONS + 4)` below the database's
`max_connections` minus ~20 for administration and replication (the manifests use 400 and 30, so
up to ~11 replicas without a pooler; raise both together).

## First deployment (Kubernetes)

1. Install the CloudNativePG operator, an ingress controller, cert-manager and metrics-server
   (and the Prometheus operator if you want the `ServiceMonitor`).
2. Create the bucket for backups and the bucket for attachments.
3. Create the secrets as shown in `deploy/k8s/secret.example.yaml`.
4. Edit `deploy/k8s/kustomization.yaml` (image), `deployment.yaml` (`PUBLIC_URL`, bucket, region),
   `ingress.yaml` (host) and `database.yaml` (backup bucket, sizes).
5. `kubectl apply -k deploy/k8s` and wait for `kubectl -n ferrum get pods` to show the database
   cluster healthy and the API pods ready (`/api/ready` checks the database).
6. Create the first tenant:

   ```bash
   kubectl -n ferrum exec deploy/ferrum -- ferrum task create_tenant slug:acme name:"Acme Ltd" email:admin@acme.com password:'<strong password>' packages:core,accounting,inventory,selling,buying,manufacturing,india
   ```

7. Optionally run `FERRUM_URL=https://<host> FERRUM_TENANT=... FERRUM_EMAIL=... FERRUM_PASSWORD=... python3 scripts/smoke.py`
   against a staging tenant (it creates a throw-away item and stock movements).
8. Sign in at `https://<host>/app/`, create the company (its chart of accounts, warehouses and,
   with `india`, GST templates are set up automatically) and a fiscal year.

Single host: `cp deploy/.env.example deploy/.env`, fill it in, then
`docker compose -f deploy/docker-compose.yml --env-file deploy/.env up -d`.

## Upgrades

* Schema changes are forward-only migrations plus a metadata → DDL sync, both run at boot under
  advisory locks, so rolling several replicas at once is safe. Upgrades only add tables, columns
  and indexes; nothing is dropped automatically (orphaned columns are reported in the logs).
* Roll forward with the image tag (`kubectl -n ferrum set image deploy/ferrum ferrum=<image>`);
  `maxUnavailable: 0` keeps capacity during the rollout.
* Rolling back the image is safe while the newer migrations are additive (the norm). If a release
  notes a data migration, take an on-demand backup first (below).

## Backups and restore

* CloudNativePG archives WAL continuously and takes a base backup daily (`ScheduledBackup`),
  keeping 30 days. On-demand backup:
  `kubectl -n ferrum cnpg backup ferrum-db` (or create a `Backup` resource).
* Point-in-time restore: create a new cluster from the object store with
  `bootstrap.recovery.recoveryTarget.targetTime: "2026-10-28 10:00:00+00"`, verify it, then point
  the `ferrum-db-app` secret / `DATABASE_URL` at it and restart the API pods.
* Docker Compose: schedule `docker compose exec db pg_dump -U ferrum -Fc ferrum > ferrum-$(date +%F).dump`
  and copy it off the host; restore with `pg_restore -d ferrum --clean`.
* Test a restore at least quarterly. After restoring, run the audit verification below.

## Monitoring

`/metrics` (Prometheus text) exposes `ferrum_up`, uptime, database pool size and idle
connections, the outbox backlog (pending, dead, age of the oldest pending event), webhook
deliveries (pending, dead) and the tenant count. Take request rates and latencies from the ingress
controller's metrics. Suggested alerts:

| Alert | Condition |
|---|---|
| API down | fewer ready pods than `minReplicas` for 5 min |
| Error rate | ingress 5xx above 1 % for 10 min |
| Pool saturation | `ferrum_db_pool_idle` at 0 for 5 min |
| Outbox lag | oldest pending event older than 5 min |
| Dead letters | any dead outbox events or webhook deliveries |
| Database | CNPG: replication lag, failover events, storage above 80 %, connections above 85 % |

Logs are JSON. Clients may send an `x-request-id` header to correlate their calls with the logs;
5xx bodies carry an `error_id` that appears in the log line with the full cause.

## Scaling

* API: the HPA scales 3–10 replicas on CPU (the connection budget above); replicas are interchangeable.
* Database: scale vertically first; add a PgBouncer pooler (CloudNativePG `Pooler`, transaction
  mode) when connections become the limit. The journal is partitioned by year (partitions for the
  next year are created automatically); stock and audit tables are append-only.
* Very large tenants or many tenants: shard on `tenant_id` (Citus), which the schema already
  allows (every key starts with `tenant_id`).

## Incident playbooks

**Database failover.** Automatic. In-flight transactions fail with a retryable error (clients see
`503 retry`, idempotent creates can be replayed with the same `Idempotency-Key`). Check
`kubectl cnpg status ferrum-db`.

**Outbox or webhook backlog growing.** Check API pod health and logs for the dispatcher. Dead
webhook deliveries usually mean the receiving endpoint is down or rejects the signature; fix the
endpoint and recreate the webhook if its secret changed.

**A plugin breaks a workflow.** Plugins fail closed: the user sees `plugin '<name>' failed: …`.
Disable it at once with `PUT /api/admin/plugins/{id}/enabled {"enabled": false}`, then fix and
re-upload (same name replaces the module).

**"Accounting period is closed".** Intended: postings into a closed month are refused. An Accounts
Manager can reopen it from the fiscal year (`reopen_period` action) if the correction must land
there; otherwise post the correction in an open period.

**User locked out.** Five failed logins lock an account for 15 minutes. Administrators can reset
the password (`PUT /api/admin/users/{id}/password`) or deactivate the user.

**Suspected token leak.** The user (or an administrator acting as them) calls
`POST /api/auth/logout_all`; revoke API keys with `DELETE /api/auth/api_keys/{id}`. Rotating
`JWT_SECRET` signs out everyone.

**Audit integrity.** Every record's history is hash-chained and anchored daily.
`GET /api/v1/{Entity}/{id}/audit` returns the trail with `chain_valid` / `broken_at`; a broken chain
means rows were changed outside the application — restore from backup and investigate database
access.

## Routine tasks

```bash
ferrum task run_maintenance          # anchor audit trails, purge expired events, ledger partitions (also runs in-process)
ferrum task sync_schema              # force the metadata → DDL sync (normally automatic at boot)
ferrum task create_tenant slug:... name:... email:... password:... packages:...
ferrum task seed_demo tenant:<slug>  # demo data (never in production tenants)
ferrum task import_erpnext src:<erpnext app dir> out:packages/erpnext-import
```
