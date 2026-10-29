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
| `FERRUM_SECRET_KEY` | yes | Base64 of exactly 32 random bytes: `openssl rand -base64 32`. Seals secret fields (mail passwords, API keys) with AES-256-GCM; every replica needs the same key. Back it up: values sealed with a lost key must be entered again |
| `FERRUM_BOOTSTRAP_TOKEN` | no | Enables `POST /api/tenants` with header `X-Ferrum-Bootstrap-Token`; empty disables it |
| `FERRUM_METRICS_TOKEN` | recommended | `/metrics` then requires `Authorization: Bearer <token>` |
| `FERRUM_S3_BUCKET`, `FERRUM_S3_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | multi-replica | Attachment storage on S3 (region `auto` if empty) |
| `FERRUM_S3_ENDPOINT` | no | Any S3-compatible store: MinIO, Cloudflare R2, Wasabi, Backblaze B2, DigitalOcean Spaces, GCS interoperability |
| `FERRUM_DIRECT_UPLOAD_MAX_BYTES` | no | Largest file browsers upload straight to the bucket (default 2 GiB). Needs the bucket CORS rule below |
| `FERRUM_PDF_URL` | no | A Gotenberg-compatible renderer (`gotenberg/gotenberg:8`, included in both deployments as `pdf` / `ferrum-pdf`) for PDFs of print formats; empty turns PDFs off. Run it with JavaScript off and an allow-list of `^(data:\|file:///tmp/).*` so templates cannot make it fetch anything; print formats then use inline (data:) images |
| `FERRUM_REQUIRE_OBJECT_STORAGE` | no | Production default `true`: refuse to start without object storage, so no replica keeps files locally. `false` only for single-server installs |
| `FERRUM_FILES_DIR` | no | Local attachment directory (default `/var/lib/ferrum/files`) |
| `FERRUM_DB_MAX_CONNECTIONS` | no | Pool size per replica (default 30) |
| `FERRUM_RUN_BACKGROUND` | no | `false` turns a replica into API-only |
| `PUBLIC_URL`, `PORT`, `BINDING`, `LOG_LEVEL` | no | Usual meanings; logs are JSON on stdout |
| `MAILER_*` | no | SMTP for notifications |
| `FERRUM_APP_STORES` | no | App stores, comma-separated base URLs (`https://store.example`); empty: no store, only apps built in or uploaded |
| `FERRUM_APP_TRUSTED_KEYS` | with stores | Comma-separated `store:<base64>` / `publisher:<base64>` Ed25519 public keys whose signatures this cluster trusts (`ferrum-app keygen` prints them). Store apps need a store countersignature by one of them; several keys may be trusted at once (rotation) |
| `FERRUM_APP_UPLOADS` | no | Who may upload app bundles: `off`, `private` (production default: sandboxed apps without tables, visible only in the uploading workspace) or `cluster` (development default: available to every workspace) |
| `FERRUM_SSO_ALLOW_PRIVATE` | no | `true` lets single sign-on reach an identity provider on a private address or over plain http (one inside your own network, such as an in-cluster Keycloak); default `false` |
| `FERRUM_APP_ALLOW_UNSIGNED` | no | `true` accepts uploads no trusted key signed (development default); production default `false` |

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
2. Create the bucket for backups and the bucket for attachments. For uploads over 25 MB, which
   browsers send straight to the attachments bucket, allow them in its CORS configuration:
   `[{"AllowedOrigins": ["https://erp.example.com"], "AllowedMethods": ["PUT"], "AllowedHeaders": ["content-type"], "MaxAgeSeconds": 3600}]`.
   Optionally expire objects under `uploads/` after a day as a backstop; the server already deletes
   abandoned uploads.
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

## Capacity

Measured on 2026-10-30 with one release build in production mode (`LOCO_ENV=production`, 30 database
connections) and PostgreSQL 17 (default settings, `synchronous_commit` on) on one laptop (Apple M1
Pro, 8 cores, 16 GB), the load generator on the same machine — so every figure is for one replica
sharing its CPU with the database and the client. No request failed in any run.

| Call | Concurrent clients | Requests/s | p50 | p95 | p99 |
|---|---|---|---|---|---|
| List 20 customers | 10 / 50 / 100 | 8,400 / 8,500 / 8,900 | 1 / 5 / 11 ms | 2 / 8 / 16 ms | 2 / 18 / 24 ms |
| Live totals of an invoice (`_calc`) | 10 / 50 / 100 | 1,550 / 1,590 / 1,590 | 6 / 24 / 55 ms | 7 / 78 / 115 ms | 8 / 143 / 174 ms |
| Create a draft Sales Invoice (2 lines, GST) | 10 / 50 / 100 / 200 | 790 / 870 / 880 / 870 | 12 / 44 / 101 / 217 ms | 15 / 164 / 219 / 334 ms | 17 / 184 / 237 / 352 ms |

Writes hold their throughput as concurrency grows; queueing shows up as latency. One replica's
event dispatcher drains about 600 outbox events a second (webhooks, automations, live updates);
dispatchers on every replica share the outbox. The test found that a NOTIFY sent by each writing
transaction serialized their commits (creates fell to 330/s at 100 clients); dispatchers now poll
instead, which adds up to 250 ms before an event is handled.

Rerun it against a staging copy after changes that touch the write path:

```bash
pip install httpx
FERRUM_URL=https://staging.example.com FERRUM_TENANT=load FERRUM_EMAIL=load@example.com FERRUM_PASSWORD=... \
  python3 tools/loadtest.py --levels 10,50,100 --seconds 60
```

The script mixes lists, reads, metadata, live totals and draft creation and reports errors and
percentiles per call; one Python client tops out near 600 requests a second, so for higher rates
run several, or `ab`/`oha` against single endpoints as above.

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

**A store revokes an app, or an app must be stopped now.** Every replica with background loops
polls each store's signed revocation list every ten minutes (one replica at a time); a revoked
release leaves the catalog, its plugin is disabled and the app is marked disabled in every
workspace that runs it — data is kept, and it cannot be enabled again until the workspace updates
to a fixed release. To act without a store: `ferrum task app_revoke app:<id> version:<v> reason:"..."`
(all versions without `version`). A list whose signature does not verify is ignored and logged
(`revocation list not applied`): check `FERRUM_APP_TRUSTED_KEYS`.

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

## Personal-data requests

A person (customer, supplier contact, employee, lead…) may ask for a copy of their data or its
erasure. Administration → Personal data finds what the installed packages declare as theirs by
email: download the JSON for an access request; for an erasure, give the request's reference as
the reason and erase. Their details leave the records listed and those records' audit history
(the events keep their place in the hash chain and record who redacted what, when and why), and
their login account is anonymized and closed. Submitted documents keep the names they carry, as
bookkeeping law requires; an administrator's account is not erased until it is no longer one. The
same is available as `GET /api/admin/personal-data` and `POST /api/admin/personal-data/erase`;
packages and apps declare their entities with `personal_data` in their manifests.

## India: e-invoices and e-way bills

With the `india` package, a submitted Sales Invoice to a registered business, an SEZ unit or a
buyer abroad gets its e-invoice from the Invoice Registration Portal (IRP); the actions are on
the invoice. Before the first one:

* **Company**: GSTIN, and under GST Registration the address, city and PIN code.
* **Customers**: GSTIN (or GST category Overseas), billing address, city and PIN code.
* **Items**: HSN/SAC codes (4–8 digits); **units of measure**: a GST unit code (UQC), or a name
  Ferrum recognises (Nos, Kg, Box, Litre…; anything else is OTH).
* **Numbering**: GST caps invoice numbers at 16 characters (the IRP: capitals, digits, `/` and
  `-`), so with `india` Sales Invoices are numbered `SI/{FY}/{seq:5}` (`SI/2026-27/00001`). A
  workspace can choose another series (a `numbering` customization of Sales Invoice) as long as it
  stays within 16; a year with more than 99,999 invoices needs a shorter prefix.

Two ways to the IRP:

* **Without a GSP**: *Download e-Invoice JSON* gives the invoice in the IRP's schema 1.1 (a list,
  as the portal's bulk upload takes it); upload it on the e-invoice portal and paste the signed
  JSON it returns into *Record e-Invoice from IRP*, which checks that the signed QR code is for
  this invoice, its number and the company's GSTIN, and records the IRN, acknowledgement and QR.
* **Through a GSP connector plugin**: *Generate e-Invoice* sets the invoice Pending and queues
  `india.e_invoice.requested` (`{invoice, number, company, payload}`); the connector sends the
  payload with its GSP credentials and answers with the `record_e_invoice` action (`{response:
  <the IRP's answer>}`, or `Irn`, `AckNo`, `AckDt`, `SignedQRCode` as parameters) or
  `record_e_invoice_failure` (`{message}`). Cancellation, e-way bills and their cancellation work
  the same way (`india.e_invoice.cancel_requested`, `india.e_way_bill.requested`,
  `india.e_way_bill.cancel_requested`; `record_e_invoice_cancellation`, `record_e_way_bill`,
  `record_e_way_bill_cancellation` and their `…_failure` actions). Recording again with the same
  answer is a no-op; a different IRN or e-way bill number is refused. No connector ships with
  Ferrum: GSP APIs differ and need a contract with the GSP.

E-way bills: *Set Transport Details* (mode, distance, vehicle or transporter, transport document)
works on submitted invoices; the e-way bill is made by IRN when the invoice has one, otherwise from
the e-way bill portal's own JSON (*Download e-Way Bill JSON* for its bulk upload). The tax invoice
print shows the IRN, the acknowledgement, the signed QR code and the e-way bill.

Cancellation: an e-invoice can be cancelled at the IRP within 24 hours of its acknowledgement
(*Cancel e-Invoice*, then *Record e-Invoice Cancellation* if done on the portal), an e-way bill
within 24 hours of being made — and the e-way bill before the e-invoice. A Sales Invoice with a
live (or pending) e-invoice or e-way bill cannot be cancelled in Ferrum; after 24 hours issue a
credit note, which gets its own e-invoice (type CRN, referring to the original).

## Sign-in security

**Two-factor sign-in.** Anyone turns it on from the account menu (Two-factor sign-in) with an
authenticator app, and keeps the ten recovery codes it shows once. Administration → Workspace →
Two-factor sign-in requires it of everyone: people without it set it up at their next sign-in.
Administration → Users → Reset 2FA is for a lost phone: it turns it off, lifts a lockout and ends
the person's sessions. Wrong codes count toward the same lockout as wrong passwords (5 in a row,
15 minutes). The secrets are sealed with `FERRUM_SECRET_KEY`.

**Single sign-on (OpenID Connect).** Register Ferrum with the identity provider as a web
application: redirect URI `https://<your host>/app/login` (Administration → Single sign-on shows
it), scopes `openid email profile`. Then add the provider there with its issuer URL, client ID and
client secret:

| Provider | Issuer URL | Notes |
|---|---|---|
| Microsoft Entra ID | `https://login.microsoftonline.com/<tenant id>/v2.0` | Entra does not say whether an address is verified: list your email domains |
| Okta | `https://<org>.okta.com` (or a custom authorization server's issuer) | |
| Google Workspace | `https://accounts.google.com` | List your domain, or any Google account with a verified address of a known person gets in |
| Keycloak | `https://<host>/realms/<realm>` | Inside your network: `FERRUM_SSO_ALLOW_PRIVATE=true` |
| Auth0 | `https://<tenant>.auth0.com/` | |

People sign in with the address the provider vouches for, and are linked to their identity there
from the first sign-in: another identity bringing the same address later is refused until an
administrator unlinks the account (Users → Unlink SSO). With "Create an account…" on, people
without an account get one with the provider's default roles; otherwise an administrator adds them
first. Workspace → Single sign-on → Require it turns password sign-in off for everyone but
administrators, who keep it to get in when the provider is down.

Removing someone at the provider stops their next sign-in, not the session they have (it lasts up
to `JWT_EXPIRATION_SECONDS`, 8 hours by default): deactivate them in Ferrum to end it at once, which
also stops their API keys.

## Routine tasks

```bash
ferrum task run_maintenance          # anchor audit trails, purge expired events, ledger partitions (also runs in-process)
ferrum task sync_schema              # force the metadata → DDL sync (normally automatic at boot)
ferrum task create_tenant slug:... name:... email:... password:... packages:...
ferrum task seed_demo tenant:<slug>  # demo data (never in production tenants)
ferrum task import_erpnext src:<erpnext app dir> out:packages/erpnext-import
ferrum task app_make_available file:<app.fapp> [allow_unsigned:true]   # make a bundle available to every workspace (verified against FERRUM_APP_TRUSTED_KEYS)
ferrum task app_upgrade [app:<id>]   # make the newest compatible store releases of the cluster's store apps (or of one app) available
ferrum task app_revoke app:<id> [version:<v>] reason:"..."   # take a release out of service and disable it everywhere (data kept)
ferrum task app_releases [app:<id>]  # every release recorded on the cluster, with status and source
```

**Apps.** A release made available on the cluster is shared by every workspace (its tables are
created once, under the cluster DDL lock); each workspace then installs, upgrades or removes it
from its Apps page with an explicit consent to the permissions shown. Bundles are kept in object
storage under `apps/<id>/<version>-<digest>.fapp` so any replica can install the plugin; the
database alone rebuilds every replica's catalog. With `FERRUM_APP_UPLOADS=cluster` a workspace
administrator's upgrade from a store also upgrades the cluster's release; otherwise store upgrades
reach the cluster through `app_upgrade`, and workspaces then move to them from their Apps page.
Native apps upgrade with the server image; each workspace applies the new roles and seed data with
"Update" on its Apps page.
