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
| `FERRUM_DRIVE_MAX_FILE_MB` | no | Largest file in one request to Drive: an upload, a new version or an editor save. Default 32, at most 1024. A replica holds each such file in memory while storing it, so allow `uploads at once × this` of memory per replica. Workspaces set their own limit within it. Raise any proxy's body limit in front with it |
| `FERRUM_REQUEST_TIMEOUT_MS` | no | How long one request may take, receiving its body included; default 120000 (2 minutes). Large files over slow links need more: allow the largest file divided by the slowest client's upload rate (200 MB at 10 Mbit/s is about 3 minutes). Keep the gateway's request timeout (600 s in `gateway.yaml`) above it |
| `FERRUM_DIRECT_UPLOAD_MAX_BYTES` | no | Largest file browsers upload straight to the bucket (default 2 GiB). Needs the bucket CORS rule below |
| `FERRUM_PDF_URL` | no | A Gotenberg-compatible renderer (`gotenberg/gotenberg:8`, included in both deployments as `pdf` / `ferrum-pdf`) for PDFs of print formats; empty turns PDFs off. Run it with JavaScript off and an allow-list of `^(data:\|file:///tmp/).*` so templates cannot make it fetch anything; print formats then use inline (data:) images |
| `FERRUM_REQUIRE_OBJECT_STORAGE` | no | Production default `true`: refuse to start without object storage, so no replica keeps files locally. `false` only for single-server installs |
| `FERRUM_FILES_DIR` | no | Local attachment directory (default `/var/lib/ferrum/files`) |
| `FERRUM_DB_MAX_CONNECTIONS` | no | Pool size per replica (default 30) |
| `FERRUM_RUN_BACKGROUND` | no | `false` turns a replica into API-only |
| `PUBLIC_URL` | recommended | The address people open, scheme and port included (`https://erp.example.com`). Apps' screens name it in their Content Security Policy, which Safari needs when a proxy in front rewrites the host, and links mailed to people outside the workspace (Drive guests) are built on it |
| `PORT`, `BINDING`, `LOG_LEVEL` | no | Usual meanings; logs are JSON on stdout |
| `MAILER_*` | no | SMTP for mail: notifications people chose to have by email (at once, or in a daily digest), and apps' mail. Without it, notifications stay in the bar and people cannot choose email |
| `FERRUM_APP_STORES` | no | App stores, comma-separated base URLs (`https://store.example`); empty: no store, only apps built in or uploaded |
| `FERRUM_APP_TRUSTED_KEYS` | with stores | Comma-separated `store:<base64>` / `publisher:<base64>` Ed25519 public keys whose signatures this cluster trusts (`ferrum-app keygen` prints them). Store apps need a store countersignature by one of them; several keys may be trusted at once (rotation) |
| `FERRUM_DEV_UI` | no | Development only: administrators may serve an app's pages from its interface's Vite server on the same machine (`ferrum-app dev --ui`, [Apps with interfaces of their own](#apps-with-interfaces-of-their-own)). The server warns at start when it is on. Never on a server people use |
| `FERRUM_PUSH_SERVICES` | no | The push services notifications go to, comma-separated: hosts (their subdomains too, over https) or exact origins. Empty: the browsers' own, Chrome's and Edge's (`fcm.googleapis.com`), Firefox's (`push.services.mozilla.com`), Safari's (`push.apple.com`) and Windows' (`notify.windows.com`); see [Notifications on devices](#notifications-on-devices-web-push) |
| `FERRUM_PUSH_CONTACT` | recommended | Who push services may contact about this server's pushes, `mailto:ops@example.com` or an `https:` page; default `PUBLIC_URL` when it is https. Apple's service wants one |
| `FERRUM_APP_UPLOADS` | no | Who may upload app bundles: `off`, `private` (production default: sandboxed apps without tables, visible only in the uploading workspace) or `cluster` (development default: available to every workspace) |
| `FERRUM_USER_REQUESTS_PER_MINUTE` | no | Requests one user (with any of their sessions and API keys) may make per minute on each replica; beyond it they get `429` with `Retry-After`. Default 3000; `0` turns it off. It counts per replica, so the cluster-wide budget grows with the replica count |
| `FERRUM_ADDRESS_REQUESTS_PER_MINUTE` | no | Requests one client address may make per minute on each replica, counted for requests that came through the proxy (`X-Forwarded-For`); default 6000, `0` turns it off. Health probes and in-cluster calls are not counted |
| `FERRUM_PROXY_HOPS` | no | How many proxies in front append to `X-Forwarded-For`: 1 (the default) for the Kubernetes gateway alone or your own TLS proxy, 0 when browsers reach the server directly (Compose sets it; set 1 there behind your proxy), 2 with a CDN or an HTTP load balancer before the proxy. The client's address for sign-in and request limits is the entry that many from the right; set it wrong and everyone shares one address (too low) or clients choose their own (too high) |
| `FERRUM_SSO_ALLOW_PRIVATE` | no | `true` lets single sign-on reach an identity provider on a private address or over plain http (one inside your own network, such as an in-cluster Keycloak); default `false` |
| `FERRUM_APP_ALLOW_UNSIGNED` | no | `true` accepts uploads no trusted key signed (development default); production default `false` |
| `FERRUM_SESSION_DAYS` | no | How long a browser stays signed in with the session cookie (default 14); each browser is a session of its own that people and administrators sign out one by one |
| `FERRUM_BASE_DOMAIN` | no | Shapes B and C (below, [Many workspaces](#many-workspaces-subdomains-and-ferrum-accounts)): the domain under which each workspace is `{slug}.{base}`, people sign in at `accounts.{base}` and outside callers use `hooks.{base}`. Empty: one workspace (`FERRUM_DEFAULT_WORKSPACE`) or a shared host |
| `FERRUM_REGION`, `FERRUM_REGIONS` | no | This cluster's region (`eu`), and the other regions' accounts hosts, `name=https://accounts.example.eu,…`: the sign-in page offers them, and a workspace not found here is looked up there |
| `FERRUM_DEFAULT_WORKSPACE` | no | The slug of the one workspace this server serves on every host (a single-workspace install): sign-in then asks for no workspace. Empty: a host registered for a workspace leads to it, and elsewhere the workspace is chosen at sign-in |
| `FERRUM_UI_DIR` | no | Interfaces registered at start-up from a directory with one directory of built files per app id (`<dir>/<app>/index.html`), for apps whose crate carries none: an interface compiled into an app's crate is the one served, and a copy of it here is skipped with a warning. The image sets it to `/app/ui`, which holds the shell's and Studio's (built from Ferrum-Frontend at the commit the Dockerfile pins); point it elsewhere to run others |
| `FERRUM_OFFICE_ORIGIN` | with online editing | The editor server as browsers reach it (`https://office.example.com`), when that is not `FERRUM_OFFICE_URL`: Drive's pages frame it and post to it, and their Content-Security-Policy names it (`deploy/office.yml` sets it from `OFFICE_PUBLIC_URL`) |
| `FERRUM_APPS_BASE` | no | A domain under which isolated apps get origins of their own, `{app}--{workspace}.{base}` (`example-apps.net`); see "Apps with interfaces of their own". Empty (the default): isolated apps run in sandboxed frames. `localhost` works in development |
| `FERRUM_UI_SOURCE` | no | Development: a server listing built interfaces (`index.json`, as Ferrum-Frontend's `npm run build:apps` writes `dist-apps/`), fetched at start-up and every ten minutes, files checked against their digests |

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

1. Install the CloudNativePG operator with its Barman Cloud plugin (backups), a Gateway API
   implementation, cert-manager with Gateway API support (`--enable-gateway-api`) and
   metrics-server (and the Prometheus operator if you want the `ServiceMonitor`).
   `deploy/kind/install.sh` installs the versions CI deploys with: CloudNativePG 1.30.1, Barman
   Cloud plugin 0.15.1, Envoy Gateway 1.9.3, cert-manager 1.21.3. ingress-nginx is not supported:
   it was retired in March 2026.
2. Create the bucket for backups and the bucket for attachments. For uploads over 25 MB, which
   browsers send straight to the attachments bucket, allow them in its CORS configuration:
   `[{"AllowedOrigins": ["https://erp.example.com"], "AllowedMethods": ["PUT"], "AllowedHeaders": ["content-type"], "MaxAgeSeconds": 3600}]`.
   Optionally expire objects under `uploads/` after a day as a backstop; the server already deletes
   abandoned uploads.
3. Create the secrets as shown in `deploy/k8s/secret.example.yaml`.
4. Edit `deploy/k8s/kustomization.yaml` (the images' tag: a commit's, see *Images* below), `deployment.yaml` (`PUBLIC_URL`, bucket, region),
   `gateway.yaml` (host; `gatewayClassName` if not Envoy Gateway's `eg`) and `database.yaml` (the
   `ObjectStore`'s backup bucket, sizes). `envoy-proxy.yaml` runs two of Envoy Gateway's proxies on
   different nodes and keeps the client's address (`externalTrafficPolicy: Local`), which the
   per-address limits count. With another gateway implementation, remove that file and the
   Gateway's `infrastructure.parametersRef`, give its proxies the same properties, and allow the
   namespace they run in (Envoy Gateway: `envoy-gateway-system`) in `networkpolicy.yaml`.
   Create the `letsencrypt` ClusterIssuer the Gateway names, with an ACME HTTP-01 solver of type
   `gatewayHTTPRoute` whose `parentRefs` is the Gateway `ferrum` in namespace `ferrum` (or use a DNS-01
   solver). With a CDN or HTTP load balancer in front of the gateway, set `FERRUM_PROXY_HOPS=2`.
5. `kubectl apply -k deploy/k8s` and wait for `kubectl -n ferrum get pods` to show the database
   cluster healthy and the API pods ready (`/api/ready` checks the database).
6. Create the first tenant:

   ```bash
   kubectl -n ferrum exec deploy/ferrum -- ferrum task create_tenant slug:acme name:"Acme Ltd" email:admin@acme.com password:'<strong password>' packages:core,accounting,inventory,selling,buying,manufacturing,india
   ```

   Without `packages:` the workspace starts with only the core, and its administrator installs
   apps from the store.

7. Optionally run `FERRUM_URL=https://<host> FERRUM_TENANT=... FERRUM_EMAIL=... FERRUM_PASSWORD=... python3 scripts/smoke.py`
   against a staging tenant (it creates a throw-away item and stock movements).
8. Sign in at `https://<host>/`, create the company (its chart of accounts, warehouses and,
   with `india`, GST templates are set up automatically) and a fiscal year.

Single host: `cp deploy/.env.example deploy/.env`, fill it in, then
`docker compose -f deploy/docker-compose.yml --env-file deploy/.env up -d`.

**Images.** For each commit on `main` whose images pass the checks below, CI publishes
`ghcr.io/cbferrum/ferrum` and `ghcr.io/cbferrum/ferrum-editors`, tagged with the commit's full SHA
and `latest`. The server's image answers every page: it carries the shell's and Studio's interfaces,
built from Ferrum-Frontend at the commit its Dockerfile pins (`FRONTEND_REV`), and apps' interfaces
come compiled into their crates; Ferrum-Frontend publishes no image of its own. On a single host,
pin them with `FERRUM_IMAGE` and `FERRUM_EDITORS_IMAGE`. Deploy a commit's tag and change it to upgrade: `latest`
moves, so pods started later (a rescheduled replica, a new node) would run another release. If
pulling needs signing in (private packages, or your own mirror), use a token that may read
packages: `docker login ghcr.io` on a single host; on Kubernetes,

```bash
kubectl -n ferrum create secret docker-registry ghcr --docker-server=ghcr.io \
  --docker-username=<user> --docker-password="$GHCR_TOKEN"
kubectl -n ferrum patch serviceaccount default -p '{"imagePullSecrets": [{"name": "ghcr"}]}'
```

**Checking an image before it goes out.** `deploy/smoke.sh` deploys the Compose stack (database,
Ferrum, PDF renderer) with freshly generated secrets. The workspace is created, seeded and
invoiced by the release being replaced (`FERRUM_PREVIOUS_IMAGE`, if set), then the server's
container is replaced with `FERRUM_IMAGE` as an upgrade does; the session and the data must
survive. On the new image it then checks the security headers; the shell's pages, apps' pages and
public pages, the store installing apps, records-only apps, OAuth consent, and today's web app's
old addresses (`/app/…`) leading to their new homes; the live-updates and live-documents
WebSockets; a Sales Invoice submitted and printed as HTML and PDF with its audit chain; recovery from a database
restart; a backup, deletion of the stack with its volumes and a restore (data, session,
attachment and numbering intact); and a short load test. Everything is removed afterwards. With
`FERRUM_EDITORS_IMAGE` it also runs the browser editors' server (`deploy/editors.yml`) and checks its
policies and its read-only filesystem. CI runs it on every image it builds, as an upgrade of `ghcr.io/cbferrum/ferrum:latest`; images are
published only when it passes. Ferrum-Frontend's CI runs it too, on a server image built with its
own checkout as the interfaces' source (`--build-context frontend=<checkout>`). To run it yourself
(Docker, curl, openssl, python3 with httpx; `SMOKE_PORT` and `SMOKE_LOAD_SECONDS` override the
port 8080 and the 20-second load test):

```bash
FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:<tag> FERRUM_PREVIOUS_IMAGE=ghcr.io/cbferrum/ferrum:latest deploy/smoke.sh
```

**Checking the Kubernetes deployment.** `deploy/kind/smoke.sh` deploys `deploy/k8s` to a cluster
(through `deploy/kind/kustomization.yaml`, which only swaps in local images and points
attachments and backups at an S3 emulator) and checks it as an operator would: three database
instances, the server and renderer ready, the Gateway serving HTTPS with a certificate from
cert-manager, HTTP redirected, no metrics outside; no pod but the gateway's and monitoring's
reaching the server (the network policy), metrics only with their token, the
autoscaler reading the API's CPU use (metrics-server); a workspace created and an invoice submitted,
printed and given an attachment that every replica serves from object storage; the database
primary killed (a standby is promoted and writes resume within 90 s — about 25 s in CI — with
nothing lost); the API restarted replica by
replica under traffic without a failed request; a backup taken and restored into a new cluster
that holds the latest writes. CI runs it on kind (a control plane and three workers) for every
image published from main (`.github/workflows/cluster.yml`). To run it yourself (Docker, kind,
kubectl, curl, python3):

```bash
kind create cluster --config deploy/kind/kind.yaml
docker pull ghcr.io/cbferrum/ferrum:<tag> && docker tag ghcr.io/cbferrum/ferrum:<tag> ferrum:ci
docker build -t ferrum-editors:ci deploy/editors
kind load docker-image ferrum:ci ferrum-editors:ci
deploy/kind/install.sh && deploy/kind/smoke.sh
```

## Upgrades

* Schema changes are forward-only migrations plus a metadata → DDL sync, both run at boot under
  advisory locks, so rolling several replicas at once is safe. Upgrades only add tables, columns
  and indexes; nothing is dropped automatically (orphaned columns are reported in the logs).
* Roll forward with the image tag (`kubectl -n ferrum set image deploy/ferrum ferrum=<image>`);
  `maxUnavailable: 0` keeps capacity during the rollout.
* Rolling back the image is safe while the newer migrations are additive (the norm). If a release
  notes a data migration, take an on-demand backup first (below).
* Some releases move existing workspaces onto a new package layout with a one-time step at start-up
  (one replica runs it; it is recorded per workspace). The first, `core-split`
  ([plan](plan/apps-ui.md) §13), comes with the release that split `core` into `core`, `parties`
  (customers, suppliers) and `products` (items, price lists), and gave ERP's packages the accounting,
  stock and buying records and fields `core` had:
  * a workspace with any of ERP's packages gets all five (accounting, inventory, buying, selling,
    manufacturing), so it keeps every record and field it had;
  * one without them gets `parties` and `products` only if it has customers, suppliers, items or
    item prices of its own;
  * tables and columns stay where they were.

  `crm-erp` comes with CRM 0.2.0, which works without ERP: a workspace with CRM and ERP's selling
  gets the connector package `crm_erp` (quotations' taxes, sales orders made from quotations, the
  orders' link to them), and contacts and addresses are the platform's `contacts`. `helpdesk-erp`
  comes with Helpdesk 0.2.0: a workspace with Helpdesk and selling gets `helpdesk_erp` (a ticket's
  sales invoice, a maintenance contract's sales order). `hr-erp` comes with HR 0.2.0: a workspace
  with HR and accounting gets `hr_erp` (expense claims and advances) and `payroll_erp` (payroll's
  accounts and postings). `projects-erp` comes with Projects 0.2.0: a workspace with Projects and
  selling gets `projects_erp` (invoicing time and milestones).

  `project-records` comes with the release that moved Project out of `core` into the shared
  `project_records` package (ERP's accounting depends on it): a workspace with projects but
  without ERP gets it.

  To go back to a release before such a step, run `ferrum task upgrade_back step:<name>`
  (`core-split`) with every replica of the newer release stopped, then start the older image. The
  task takes out the packages the step installed and keeps the data.

## Backups and restore

* CloudNativePG archives WAL continuously through the Barman Cloud plugin and takes a base backup
  daily (`ScheduledBackup`), keeping 30 days (the `ObjectStore`'s `retentionPolicy`). On-demand
  backup: `kubectl -n ferrum cnpg backup ferrum-db --method plugin --plugin-name barman-cloud.cloudnative-pg.io`
  (or create a `Backup` resource with `method: plugin`).
* Point-in-time restore: create a new cluster recovering from the object store, shaped like
  `deploy/kind/restore.yaml` (`bootstrap.recovery.source` names an external cluster read through
  the plugin, `barmanObjectName: ferrum-backups`, `serverName: ferrum-db`), adding
  `recoveryTarget: {targetTime: "2026-10-28 10:00:00+00"}`; verify it, then point the
  `ferrum-db-app` secret / `DATABASE_URL` at it and restart the API pods. CI restores every image's
  backup this way.
* Clusters set up with the earlier manifests (`spec.backup.barmanObjectStore`, removed in
  CloudNativePG 1.31): install the plugin, then apply the new `database.yaml`. The `ObjectStore`
  takes the same `destinationPath`, so earlier backups and WAL stay usable for recovery.
* Docker Compose (commands run in `deploy/`): schedule a dump of the database as its superuser —
  as `ferrum` it fails, since that role is held to the workspaces' row-level security, and
  leaves a truncated file — and an archive of the attachments volume, and copy both off the host:

  ```bash
  docker compose exec -T db pg_dump -U postgres -Fc ferrum > ferrum-$(date +%F).dump
  docker compose exec -T ferrum tar -C /var/lib/ferrum/files -czf - . > files-$(date +%F).tgz
  ```

  To restore onto a new host, start the database alone (`docker compose up -d db`), wait until
  `docker compose exec db pg_isready -h 127.0.0.1` succeeds, then:

  ```bash
  docker compose exec -T db pg_restore -U postgres -d ferrum --clean --if-exists --exit-on-error < ferrum.dump
  docker compose run --rm --no-deps -T --entrypoint tar ferrum -C /var/lib/ferrum/files -xzf - < files.tgz
  docker compose up -d
  ```

  Restoring in place, `docker compose stop ferrum` first.
* Test a restore at least quarterly. After restoring, run the audit verification below.

## Monitoring

`/metrics` (Prometheus text) exposes `ferrum_up`, uptime, database pool size and idle
connections, the outbox backlog (pending, dead, age of the oldest pending event), webhook
deliveries (pending, dead) and the tenant count. Take request rates and latencies from the gateway
controller's metrics. Suggested alerts:

| Alert | Condition |
|---|---|
| API down | fewer ready pods than `minReplicas` for 5 min |
| Error rate | gateway 5xx above 1 % for 10 min |
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
run several, or `ab`/`oha` against single endpoints as above. It signs in as one user: raise that
server's `FERRUM_USER_REQUESTS_PER_MINUTE` (or set it to `0`) for the run, or the per-user budget
answers `429` long before the server is busy; through the gateway, raise
`FERRUM_ADDRESS_REQUESTS_PER_MINUTE` too.

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

## Online editing

Drive opens documents, spreadsheets and presentations in an editor server you run beside Ferrum:
Collabora Online, or ONLYOFFICE Docs in WOPI mode. The editor reads and saves through Drive's WOPI
endpoints with each person's own rights, and every save becomes a new version. Details are in the
Drive app's README.

| Setting | On | Value |
|---|---|---|
| `FERRUM_OFFICE_URL` | server | the editor as the server reaches it, e.g. `http://office:9980` |
| `FERRUM_OFFICE_WOPI_URL` | server | the server as the editor reaches it, e.g. `http://ferrum:5150` |
| `FERRUM_OFFICE_IPS` | server | optional: the only addresses WOPI calls are accepted from |
| `FERRUM_OFFICE_ORIGIN` | server | the editor as browsers reach it; Drive's pages may frame it and post to it |

**Compose.** `deploy/office.yml` adds Collabora:

    docker compose -f deploy/docker-compose.yml -f deploy/office.yml --env-file deploy/.env up -d

Set `OFFICE_PUBLIC_URL` and `OFFICE_PUBLIC_HOST` to the editor's public address.

**Kubernetes.** Run Collabora with its own Helm chart (`collabora-online`). Then:
* give it `aliasgroup1` (Ferrum's in-cluster URL) and `net.frame_ancestors` (Ferrum's URL);
* set the four values above on the server;
* let only the editor's pods reach `/api/site/*/drive/wopi/` (network policy).

**Things to know**
* **People in the editor:** the editor tells Drive's page who is in a file. Its Share, version
  history and close buttons open Drive's own, through messages to Ferrum's origin. The server
  names that origin from its `PUBLIC_URL`, which must be the address people open: without it, the
  editor keeps its own buttons.
* **Production terms:** CODE, the image `deploy/office.yml` uses, is a development edition.
  Production use means a Collabora Online subscription or a build from source.
* **Proof key:** give the editor one, and every WOPI call must carry its signature over the token,
  the URL and the time, so a leaked token alone opens nothing. `deploy/office.yml` mounts
  `deploy/office-proof-key`; make it with `ssh-keygen -t rsa -N "" -m PEM -f deploy/office-proof-key`.
  On Kubernetes, mount it at `/etc/coolwsd/proof_key` from a secret.
* **Without a proof key**, Ferrum relies on the tokens (per person and file, ten hours) and on the
  network: keep the WOPI endpoints reachable only from the editor (`FERRUM_OFFICE_IPS` or network
  policy).
* **Logs:** request logs record paths without query strings, so WOPI tokens and sign-in codes never
  reach them.
* **Size:** saves are limited by Drive's upload limit, at most the server's
  `FERRUM_DRIVE_MAX_FILE_MB` (32 MB unless raised). Editor saves go straight to the server.
* **Rate limits:** the editor calls Ferrum from one address, so the public endpoints' per-address
  limit applies to all of its sessions together. Raise it for busy editors, or exempt the editor's
  address at the ingress.

## Browser editors

Drive's add-on Browser editors opens spreadsheets, documents and PDFs in editors that run in the
person's browser: GridCraft (xlsx, csv, tsv), WordCraft (docx, odt, rtf) and PdfCraft (pdf). The
plan is [docs/plan/browser-editors.md](plan/browser-editors.md).
* You run the editor server, `deploy/editors`, image `ghcr.io/cbferrum/ferrum-editors`. It holds
  the editors' official builds, unmodified and checked against pinned checksums, and a small relay
  page.
* Drive's page downloads the file, hands it to the editor through the relay, and saves what the
  editor returns as a new version through Drive, with the person's rights.
* Files never reach the editor server. It holds no data and no secrets, and serves static files
  only.

The editor server needs an origin of its own: another host name, or on a single host another port.
The editors must never share Ferrum's origin, where they could reach people's sessions.

| Setting | On | Value |
|---|---|---|
| `FERRUM_EDITORS_URL` | server | the editor server as browsers reach it, e.g. `https://editors.example.com` |
| `FERRUM_PUBLIC_URL` | editor server | Ferrum's address, the only page that may frame the relay (several separated by spaces) |

Each workspace that wants the editors then installs **Browser editors** from its Apps page (under
Drive). Without the add-on, or with `FERRUM_EDITORS_URL` empty, Drive offers no browser editors.

**Compose.** `deploy/editors.yml` adds the editor server on port 8090:

    docker compose -f deploy/docker-compose.yml -f deploy/editors.yml --env-file deploy/.env up -d

Set `FERRUM_EDITORS_URL` in `deploy/.env`. Copy and paste inside the editors need a secure page:
HTTPS, or `localhost`.

**Kubernetes.**
1. List the component `editors` under `components:` in `deploy/k8s/kustomization.yaml`.
2. Replace `editors.example.com` and `erp.example.com` in `deploy/k8s/editors/` with your hosts.

The component adds:
* the editor server: two replicas, read-only and without capabilities;
* a Gateway listener with its own certificate, and the routes;
* a network policy: only the gateway reaches the editor server, and it reaches nothing;
* `FERRUM_EDITORS_URL` on the server.

**Things to know**
* **One editor at a time.** Browser and online editor sessions share each file's lock.
  * While someone edits, others open the file read-only, and uploads and restores of that file wait.
  * A lock lapses 30 minutes after the editor last refreshed it, for example after a browser crash.
* **Conflicts.** A save made from an older version than the file's current one is refused. The
  person saves a copy or replaces the file, and the other change stays in the history.
* **Upgrades.** `deploy/editors/builds.txt` pins each editor's version and checksum.
  * Moving a pin is a reviewed change: read the editor's release notes, and let CI's browser tests
    pass in Chromium, Firefox and WebKit.
  * Open editors keep the version they loaded.
* **Size.** Editors hold the whole file in the browser's memory.
  * The add-on caps what it offers them: 50 MB for sheets and documents, 100 MB for PDFs.
  * Larger files open in the online editor, or download.
* **What browsers need:** WebGL, available in current desktop browsers. On phones the editors work,
  but they are made for a mouse and keyboard.
* **Accessibility:** the editors draw on a canvas and offer little to screen readers. The online
  editor stays available to everyone through "Open with".
* **Saves under another name.** WordCraft and PdfCraft show their own File menu, with Open and
  New. Only a file saved under the opened name becomes a new version of it. Anything else they
  hand back (Save As, an export, another file) is saved as a new file beside it.
* **Names:** the add-on and its listing do not use the ArtCraft name or logos (their brand terms).
  The editors show their own.

## Hosted mail

The Mail, Chat and Calendar app (Comms) can give people mailboxes on a Stalwart mail server that
you run beside Ferrum (Stalwart 1.0, open source). Ferrum creates, moves, locks and unlocks the
accounts so that they follow the people. The app's README ("Hosted mail") has the details.

| Setting | On | Value |
|---|---|---|
| `FERRUM_MAIL_URL` | server | Stalwart's HTTP address as the server reaches it, e.g. `http://mail:8080` |
| `FERRUM_MAIL_ADMIN`, `FERRUM_MAIL_ADMIN_SECRET` | server | a Stalwart administrator |
| `FERRUM_MAIL_HOST` | server | optional: the server's public host name, e.g. `mail.example.com`, shown to people setting up mail programs (IMAP 993, SMTP 465) |

**Compose.** `deploy/mail.yml` adds Stalwart:

    docker compose -f deploy/docker-compose.yml -f deploy/mail.yml --env-file deploy/.env up -d

1. `docker compose ... logs mail` shows a temporary administrator, once.
2. Sign in with it at `http://localhost:8081/admin` on the host; from elsewhere, use an SSH
   tunnel, because the port is published on the host only. Complete the setup there:
   * the server's host name, e.g. `mail.example.com`;
   * a first domain;
   * TLS certificates (ACME).

   The setup gives you the permanent administrator.
3. Put that administrator in `deploy/.env` as `FERRUM_MAIL_ADMIN` and `FERRUM_MAIL_ADMIN_SECRET`,
   then run `up -d` again. Stalwart keeps its setup and mail in the `mail-config` and `mail-data`
   volumes; back them up with the database.
4. Publish the DNS records Stalwart lists for each domain under Domains: MX, SPF, DKIM, DMARC
   and the rest. Add reverse DNS (PTR) for the server's address.
5. In each workspace:
   * install Mail, Chat and Calendar;
   * set Communication Settings → Mail Domain;
   * then run Sync Mailboxes, or wait for the next run (every ten minutes).

**Kubernetes.** Run Stalwart as a StatefulSet:
* persistent volumes at `/etc/stalwart` and `/var/lib/stalwart`;
* a LoadBalancer service for ports 25, 465, 587 and 993;
* the three values above on the server, from a secret;
* an egress rule in the server's network policy for Stalwart's port 8080. The policy lets the
  server's pods reach only the database, the PDF renderer, DNS and ports 443 and 587.

**Things to know**
* **Outgoing mail:** many clouds block outgoing port 25, and a new address has no reputation.
  Route outgoing mail through a relay such as Amazon SES or Postmark (Stalwart: outbound routing).
* **Shared server:** several workspaces can share one server.
  * Each workspace has its own domain, and each account carries
    `externalId = ferrum:<workspace>:<person>`.
  * Ordinary users cannot list other accounts: Stalwart ships with directory queries off. Keep
    them off.
  * For hard separation, run one Stalwart per Ferrum deployment, or use the Enterprise edition,
    which has tenants.
* **Passwords:** each account's password is random and sealed in Ferrum, and nobody is shown it.
  * People read their mail in Ferrum's webmail, which signs in for them.
  * Mail programs use app passwords that people make there.
  * Locking a person replaces every password of the account, app passwords included.
* **Accounts Ferrum does not know:** an existing account at a person's address, made by hand or
  left from another workspace, is never taken over. Sync Mailboxes reports it under `failed`.

## Meetings

The Mail, Chat and Calendar app (Comms) has video meetings: a call in any chat channel, a meeting
for any calendar event, and rooms people start from Meet. They run on a
[LiveKit](https://livekit.io) server (Apache-2.0) that you run beside Ferrum. The plan is
[docs/plan/collaboration.md](plan/collaboration.md), C4.
* Browsers connect to LiveKit directly: signalling over a WebSocket, sound and pictures over UDP
  (or TCP). Nothing of a call passes through Ferrum.
* Ferrum decides who may join which meeting and signs a LiveKit access token for them, with an API
  key and secret that you give both. It never calls LiveKit.
* Who may join: a channel's call, whoever may read the channel; an event's meeting, its organizer
  and guests (anyone for a public event); a room, anyone in the workspace with its link. The app's
  README ("Meetings") has the details.

| Setting | On | Value |
|---|---|---|
| `FERRUM_MEET_URL` | server | LiveKit as browsers reach it, `wss://meet.example.com` |
| `FERRUM_MEET_KEY`, `FERRUM_MEET_SECRET` | server | an API key (a name) and its secret, at least 32 characters (`openssl rand -hex 32`); LiveKit gets the same pair |

With the three set, Chat shows a call button, Calendar offers "Add video meeting" to organizers,
and Meet appears under Communication. Only people with a Comms role can join.

**Compose.** `deploy/meet.yml` adds LiveKit:

    docker compose -f deploy/docker-compose.yml -f deploy/meet.yml --env-file deploy/.env up -d

* Put a TLS proxy in front of port 7880 (WebSocket) and set `FERRUM_MEET_URL` to its `wss://`
  address. A page served over HTTPS cannot use `ws://`.
* Open UDP 7882 and TCP 7881 to the internet: they carry the media. They must reach the host
  directly, not through the proxy.
* LiveKit tells browsers the host's address, found by STUN. Set `FERRUM_MEET_NODE_IP` to give it
  yourself, and `FERRUM_MEET_EXTERNAL_IP=false` on a network without internet access.
* To try it on one machine: `FERRUM_MEET_URL=ws://localhost:7880`, `FERRUM_MEET_NODE_IP=127.0.0.1`,
  `FERRUM_MEET_EXTERNAL_IP=false`.

`deploy/smoke.sh` with `SMOKE_MEET=1` runs LiveKit beside the stack. It checks that LiveKit accepts
the passes Ferrum signs, refuses one signed with another secret, and that Meet's pages may reach
it. CI runs it on every image.

**Kubernetes.** Run LiveKit with its own Helm chart (`livekit/livekit-server`), as its
documentation describes: host networking for the media ports, its load balancer or ingress for the
WebSocket with TLS, and Redis when you run more than one LiveKit server. Then:
* give it the key and secret, and put the three settings on Ferrum's Deployment from a secret
  (Comms' pages may then connect to LiveKit).

The server's network policy needs no change: Ferrum never calls LiveKit.

**Things to know**
* **Capacity:** LiveKit forwards streams without mixing them, so its load grows with the streams
  each person receives. Size it from LiveKit's own benchmarks and watch its CPU
  (`prometheus_port` in its configuration exposes metrics). More people need more LiveKit servers,
  with Redis.
* **Restrictive networks:** where UDP is blocked, browsers fall back to TCP 7881. Where only HTTPS
  is allowed, turn on LiveKit's TURN server with a certificate (its `turn` settings).
* **Access:** a token admits its holder for 6 hours. Someone who loses access, for example removed
  from a private channel, gets no new token, but stays in a call they are already in until they
  leave it.
* **Privacy:** calls are encrypted between browsers and LiveKit, which sees the media. Ferrum
  records nothing. Recording and end-to-end encryption are not built.
* **Several workspaces** can share one LiveKit server: rooms are named after the workspace.
* **Guests.** People outside the workspace join by link, as "Name (guest)": an event's outside guests
  by the personal link their invitation carries (when the event's meeting is here), anyone by a
  room's guest link (Invite guests on the room's page). Removing a guest from the event, or turning
  the room's link off, stops the link. Guests need nothing but the link; there is no waiting room.

## The assistant

The bar's Assistant (the sparkles button; `/assistant/` on a phone) answers questions about the
workspace and does things in it, as the person, with exactly their permissions: it uses the tools
AI agents get over MCP (below, "AI agents"), on Claude through Anthropic's Messages API. It reads
at once; every change (creating, updating, submitting, cancelling, deleting records, running
actions, installing apps) is shown to the person, who allows or refuses it before it runs.
Conversations are kept on the server (`assistant_thread`, `assistant_message`), so an answer goes
on when the page that asked moves to another app, and any page shows it again.

It is off unless you set a key:

| Setting | Value |
|---|---|
| `FERRUM_ASSISTANT_API_KEY` | an Anthropic API key; without it there is no assistant |
| `FERRUM_ASSISTANT_MODEL` | default `claude-opus-5-5` |
| `FERRUM_ASSISTANT_EFFORT` | `low`, `medium` (default), `high`, `xhigh` or `max`: how hard the model thinks, and so its speed and cost |
| `FERRUM_ASSISTANT_URL` | default `https://api.anthropic.com`; a proxy or gateway that speaks the same API |
| `FERRUM_ASSISTANT_FALLBACKS` | `on`/`off`; on by default for Anthropic's API with a model that has them: a request the model's safety checks decline is answered by Anthropic's recommended other model in the same call |
| `FERRUM_ASSISTANT_CALLS_PER_HOUR` | calls to the model one person may make in an hour, default 120 (a question that reads several things is several calls) |

* **Checking it.** After setting the key, `ferrum task assistant_check` sends the model one short
  message (no tools, no workspace data) and says which model answered, or why the key, the model
  or the network does not work.
* **What leaves the server.** The person's messages, the page they are on, and what the tools
  read for an answer (records, reports) go to the model's API, and nothing else: the assistant
  sees only what the person may see. Set the key only where your data policy allows it; Anthropic's
  API terms and retention apply to what is sent.
* **Per workspace.** Administrators turn it off for their workspace from the panel's
  Conversations menu (or `PUT /api/admin/settings {"assistant": false}`); every replica follows
  within 20 seconds. Guests and portal people never have it.
* **Limits.** 25 calls to the model per message (then it stops and says so), 400 messages per
  conversation, the hourly allowance above, one answer at a time per conversation, and tool
  results cut at 150,000 characters (with a note to the model saying so). Rate limits and
  overloads at the API are retried three times with backoff; a key the API refuses is logged as an
  error and shown to people as "not set up correctly".
* **Replicas.** An answer runs on the replica that took the message; a page on another replica
  shows it when it is stored (it asks every few seconds). An answer whose replica went away is
  taken as stopped after 15 minutes, and calls it left half done are reported to the model as
  interrupted.
* **Deleting.** People delete their conversations from the Conversations menu; removing a
  workspace removes its conversations.

## Live documents

Drive's documents (New → Document) are edited by everyone who has them open at once, with each
other's cursors, in Docs. Nothing extra runs.
* **Where they live.** A replica keeps the documents its clients have open in memory and stores
  each change in PostgreSQL (`coedit_doc`, `coedit_update`) within 50 ms. Replicas tell each other
  over LISTEN/NOTIFY (`ferrum_coedit`), as for live updates, so any replica serves anyone: no sticky
  sessions. Every 500 changes are folded into a snapshot.
* **Proxies** must pass WebSockets on `/ws/` with read timeouts above 30 seconds (the server pings
  every 30). The web container and the Gateway do.
* **Saving to Drive.** A new version is written when the last editor on a replica leaves, and every
  ten minutes while people type, only when the content changed. A sweep on every replica saves, within
  about fifteen minutes, documents a stopped replica left unsaved.
* **Failures.** A change lives in its editor's tab until it is stored. Clients reconnect on their
  own and resend what the server lacks. The editor shows Saving… or Saved, and warns before a tab
  with unsaved changes closes.
* **Limits.** A document grows to 32 MB at most (deleting always works). One message is at most 8 MB.
* **Exports.** PDF needs the PDF renderer (`FERRUM_PDF_URL`); HTML and text always work.
* The CRDT library is built from `third_party/yrs`: yrs 0.28.0 with a fix for emoji, until a
  release includes it (`third_party/yrs/FERRUM.md`).

## Calendar and contacts apps

People can sync their Ferrum calendar and contacts with the apps of their phones and computers
(CalDAV and CardDAV): Apple Calendar and Contacts, Thunderbird, DAVx⁵ on Android, Outlook through a
CalDAV add-in. Nothing extra runs: the server answers them under `/api/dav/comms/`, for workspaces
with Mail, Chat and Calendar installed.
* **Signing in.** Apps send the person's email and an app password over HTTP Basic, so serve Ferrum
  over HTTPS. App passwords are API keys that work only for these apps; people make and revoke them
  under their menu → Calendar and contacts apps. Wrong passwords count against the address like
  failed sign-ins.
* **Finding the server.** Apps look for `/.well-known/caldav` and `/.well-known/carddav` on the
  host people enter; Ferrum redirects them to `/api/dav/comms/`. The web container and the
  Kubernetes Gateway pass both to the server; a proxy of your own in front must too. With DNS SRV
  records (`_caldavs._tcp`, `_carddavs._tcp`) people can enter just the domain.
* **What syncs.** Each person's calendar (events they organize or are invited to) both ways; the
  address books Colleagues and Contacts (CRM) read-only. Comms' README ("Calendar and contacts
  apps") has the details.

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

Three ways to the IRP:

* **The direct connection**: under the company's *e-Invoice Connection*, choose Sandbox or
  Production and enter the API address (the one NIC gives at registration — its published
  sandbox site serves documentation, not the API — or your GSP's when it passes NIC's API
  through), the client ID and secret, the
  API user name and password, the IRP's public key (PEM) and, for a GSP, the extra headers it
  wants (JSON). *Generate e-Invoice* then registers the invoice within seconds: Ferrum signs in
  (one session per company and replica, renewed before its six hours run out), sends the invoice
  encrypted, and records the answer — or the IRP's reason for refusing it on the invoice
  (*e-Invoice Error*). When an answer is lost on the way back, the retry finds the IRN the IRP
  already made and fetches it; when the IRP cannot be reached, the request is retried with
  backoff. Cancellations and e-way bills by IRN go the same way. Signed QR codes NIC issued are
  checked against NIC's published signing keys, which Ferrum ships; a private IRP's are checked
  when you enter its signing key (PEM) as the company's IRP QR key, which also overrides NIC's.
  When NIC publishes a new key (einvoice1.gst.gov.in/Others/Publickeys; the current ones run to
  1 May 2027), recording a QR code by hand under it fails with a message naming its key until
  Ferrum ships the key or you enter it as the company's IRP QR key; codes from the direct
  connection keep being recorded, with a warning in the log.

  Through a GSP that passes NIC's API through, the API address is the GSP's and *GSP Headers*
  holds its own credentials as a JSON object of header names and text values, for example
  `{"Ocp-Apim-Subscription-Key": "…", "gstin": "27…"}`. They go on every call (sign-in,
  registration, fetching by IRN, cancellation, e-way bills) once each; one named like a header
  the IRP takes (`gstin`, `client_id`) replaces it. A name that is not a valid HTTP header, or a
  value that is not text, fails the request with the header's name (never its value) instead of
  being retried, and the GSP's own refusal (an HTTP 401 with its message, say) is recorded with its
  status and words. A changed address or API user signs in afresh.
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
  answer is a no-op; a different IRN or e-way bill number is refused. Use this for a GSP whose
  API is its own rather than NIC's.

E-way bills: *Set Transport Details* (mode, distance, vehicle or transporter, transport document)
works on submitted invoices; the e-way bill is made by IRN when the invoice has one, otherwise from
the e-way bill portal's own JSON (*Download e-Way Bill JSON* for its bulk upload). The tax invoice
print shows the IRN, the acknowledgement, the signed QR code and the e-way bill.

Cancellation: an e-invoice can be cancelled at the IRP within 24 hours of its acknowledgement
(*Cancel e-Invoice*, then *Record e-Invoice Cancellation* if done on the portal), an e-way bill
within 24 hours of being made — and the e-way bill before the e-invoice. A Sales Invoice with a
live (or pending) e-invoice or e-way bill cannot be cancelled in Ferrum; after 24 hours issue a
credit note, which gets its own e-invoice (type CRN, referring to the original).

## Apps with interfaces of their own

Apps may own whole interfaces at addresses of their own (`/deals/…`, [plan](plan/apps-ui.md)).
The server answers those pages itself, with the shell put in: the bootstrap, the workspace's theme,
the bar runtime (`/_shell/…`) and the app's files (`/_ui/…`).

* **Routing.** Send the server everything: every page, `/api/`, `/ws/` and `/files/` (long-lived
  WebSockets on `/ws/` and `/mcp`). A proxy or CDN may cache `/_ui/` for good (the paths are
  content digests) and `/_shell/` briefly. `/healthz` answers `ok` for load balancers.
* **The shell, for every workspace.** Home at `/`, ERP at `/erp/`, the store, administration and
  the account at `/store/`, `/admin/`, `/account/`, records at `/data/` (an app without an
  interface of its own at `/data/{app}/`, Customize at `/data/customize/`). Today's web app has
  left: its addresses (`/app/…`, in bookmarks and links in mail) lead to their new homes, and its
  offline worker removes itself. The `shell_v2` setting no longer matters; it is kept for going
  back to the release before, which still had today's web app.
* **Comms, Drive and Docs** have interfaces of their own: Mail,
  Chat, Calendar and Meet at `/mail/`, `/chat/`, `/calendar/`, `/meet/`; Drive at `/drive/` with
  Sheets, Slides and PDF; Docs at `/docs/`. Each app's crate carries its interface, registered at
  start-up; nothing to configure. Their old addresses (`/app/drive…`, bookmarks and links in mail
  included) lead there, and the pages people outside the workspace open by link are those apps'
  public pages (`/drive/s/…`, `/meet/guest/…`). Meetings need `FERRUM_MEET_URL` on the
  server (calls connect there); online editing `FERRUM_OFFICE_ORIGIN` (above).
* **Sessions.** Browsers sign in with an `HttpOnly` cookie, `__Host-ferrum` over https. The cookie
  is scoped to the host, so serve each workspace on one host, and set `PUBLIC_URL` to it.
* **Interface files** are kept in the object storage, under `ui/{app}/{digest}/`, for 30 days after
  no workspace uses them, so tabs open across an upgrade keep loading theirs.
* **Who may bring one.** An interface runs on the workspace's host with the person's session for
  apps compiled in, bundles signed by a key in `FERRUM_APP_TRUSTED_KEYS`, and uploads for the whole
  cluster (`FERRUM_APP_UPLOADS=cluster`). An unsigned app uploaded to one workspace
  (`FERRUM_APP_UPLOADS=private`, the default, with `FERRUM_APP_ALLOW_UNSIGNED=true`) runs its
  interface isolated: in a sandboxed frame without the session, reaching the API only through the
  bridge or a five-minute app token, within what the app declared. The install dialog says so.
* **Origins of their own for isolated apps** (optional, `FERRUM_APPS_BASE`). Give the apps a
  domain that is a site of its own: another registrable domain (`example-apps.net`, not
  `apps.example.com` when the workspaces are under `example.com`) or one in the Public Suffix List,
  so browsers keep their pages in processes of their own and the server's same-site checks hold.
  Point `*.{base}` at the server in DNS, and give the gateway a listener for `*.{base}` with a
  wildcard certificate (cert-manager issues those with a DNS-01 solver) routing everything to the
  server as for the workspace's host. The server tells the app and the workspace from the host; there
  it serves the app's files, the API its app tokens reach and the token endpoint, and never reads or
  sets cookies. An app whose `{app}--{workspace}` is longer than 63 characters keeps the sandboxed
  frame. A workspace's slug names its apps' origins, so the database never hands a slug to another
  workspace: a renamed or deleted workspace's slug is retired for good.
* **Size budgets.** An interface's first script (its entry and the modules preloaded with it) is at
  most 200 KB compressed and its CSS 50 KB; pages load when opened. Ferrum-Frontend's build and each
  app's `check` (`packages/build`) fail over budget, so a release cannot grow past it unnoticed.
* **Developing an interface.** On a development server run with `FERRUM_DEV_UI=1`: in the app's
  `ui/`, `npm run dev` starts Vite (`packages/build/cli.mjs dev`), and `ferrum-app dev --ui
  http://localhost:5173 --no-upload` serves the app's pages from it behind the real session, bar
  and bootstrap, with hot reload, until `--ui off` or a restart. `ferrum-app new <id> --ui` makes an
  app with an interface to start from.
* **The portal** ([plan](plan/apps-ui.md) §9.1) is at `/portal/` on the workspace's host, and at
  the root of a host whose target is `portal` (`support.example.com`): the pages apps give
  customers and suppliers (Helpdesk's "Your tickets", ERP's "Your invoices"). Administrators add
  portal people under Administration → Users (kind *Portal person*, or *Make portal person*); they
  hold no roles and sign in with a code sent to their email, so the portal needs `MAILER_*`.
* **Public sites.** A host whose target is `site:{app}` (`shop.example.com` → `site:commerce`) is
  that app's public site at its root, without the shell or sessions: Commerce's storefront, with
  its own `robots.txt` and `sitemap.xml`. Point the domain at the server like the workspace's.
  Administrators add such hosts themselves (below).

## Notifications on devices (Web Push)

People turn notifications on per device in the bell's settings, and kinds they set to "On my
devices" then pop up there, even with no Ferrum page open (docs/plan/apps-ui.md §12.5). The server
sends each one to the browser's push service, encrypted for that device (the service cannot read
it) and signed with the server's VAPID key.

* **Nothing to configure** beyond `FERRUM_SECRET_KEY`. The VAPID key is made by the first replica
  that needs it, sealed with the secret key in the database (`server_key`), and shared by every
  replica. Without a secret key there is no Web Push; the bell does not offer it.
* **Outbound access.** Replicas reach the push services over https: `fcm.googleapis.com`,
  `updates.push.services.mozilla.com`, `web.push.apple.com` and `*.notify.windows.com`. Where egress
  is filtered, allow those; a push service that cannot be reached is tried again with the outbox's
  backoff (up to 15 times over about 6 hours), and the bar has the notification meanwhile.
* **Only push services.** The server sends to no address but those (`FERRUM_PUSH_SERVICES`), each
  checked again when sending (no private addresses), so a subscription cannot point it elsewhere.
* **A new secret key** cannot open the stored VAPID key: the server logs an error, makes a new one,
  and forgets subscriptions made with the old one as they come up. People's browsers subscribe
  again on their next visit without asking (the permission stays).
* **Sign-out ends it.** A device's subscription belongs to its browser session: signing that device
  out, signing out everywhere, deactivating the person or the session expiring stops its pushes.
* **Installing.** Every page links the workspace's manifest (`/manifest.webmanifest`), so browsers
  offer to install it as one app; an app that asks (`installable: true` on its `shell.app` entry,
  Point of Sale does) is offered on its own pages as an app of its own. Installed apps show the
  unread count on their icon where the system supports it.

## Custom domains

Administrators give a workspace its own addresses under Administration → Domains
([plan](plan/apps-ui.md) §7): for the workspace (`shell`), its portal (`portal`) or an app's public
site (`site:{app}`), at most 20 per workspace.

* **Pointing a domain**: a CNAME record to the cluster's address (the host of `PUBLIC_URL`, shown on
  the page), or A/AAAA records to the same addresses for an apex domain.
* **Verification**: once a minute one replica checks up to six domains that are due: pending ones every
  five minutes, working ones daily. A domain works when `https://{domain}/.well-known/ferrum-domain`
  answers its token, which the server gives to whoever asks on that host; it stops working after seven
  failed checks in a row (a week of daily checks). Administrators can check one at once. The check
  refuses private addresses and follows no redirect; with `settings.webhooks_allow_private` (development)
  it uses plain http.
* **Certificates**: the edge in front of Ferrum gets one per domain. With Caddy's on-demand TLS:

  ```caddyfile
  {
    on_demand_tls {
      ask http://ferrum:5150/_tls/ask
    }
  }
  https:// {
    tls {
      on_demand
    }
    reverse_proxy ferrum:5150
  }
  ```

  `/_tls/ask?domain=` answers 200 only for a workspace's subdomain, a working custom domain, one being
  verified, or a removed one still redirecting, so nobody can make the edge request certificates for
  other names. On Kubernetes, put Caddy (or another edge with on-demand TLS) in front of the
  `ferrum` service; with cert-manager instead, each custom domain needs its own Ingress and
  Certificate, which an operator adds when the domain is verified.
* **Removing** a working domain releases it: it redirects to the workspace's main address of the same
  kind for 90 days, and no workspace can claim it for a year, so old links and data in browsers never
  reach a new owner. A domain that never worked is simply dropped.
* **Main address**: one working domain of each kind is the main one: links in mail and redirects use
  it, and the workspace's other addresses of that kind lead to it.

## Many workspaces: subdomains and Ferrum accounts

With `FERRUM_BASE_DOMAIN` (shapes B and C of [the plan](plan/apps-ui.md) §7), a cluster serves many
workspaces, each at its own address, and people sign in once for all of them ([API](api.md#ferrum-accounts--the-accounts-host-shapes-b-and-c)):

* **Addresses.** `{slug}.{base}` for each workspace (added at start-up for those without one, and when a
  workspace is made), `accounts.{base}` for signing in, the account chooser and each person's own
  security, and `hooks.{base}` for outside callers (webhooks and other calls that name their workspace
  in the path; no pages, no sessions). Any other host answers 404, so DNS left pointing at the cluster
  shows nobody's workspace. The slugs `accounts`, `hooks`, `www` and `api` are kept; a custom domain
  under the base domain is refused. Workspaces' verified custom domains ([Custom domains](#custom-domains))
  sign in through the accounts host as well, each with a session of its own.
* **DNS and certificates.** A wildcard record `*.{base}` to the edge, and a wildcard certificate:
  cert-manager needs a DNS-01 solver for it (a Gateway listener with hostname `*.{base}` and the
  `cert-manager.io/cluster-issuer` annotation, as `deploy/kind/kustomization.yaml` adds for the test
  cluster). Caddy's on-demand TLS above covers subdomains and custom domains alike (`/_tls/ask`
  answers for both). Put the base domain on the Public Suffix List once you can, so each workspace's
  host is a site of its own for browsers.
* **Ferrum accounts.** The first time someone signs in at the accounts host with a workspace's
  password, that workspace's account makes their Ferrum account (it takes its password, authenticator
  app and recovery codes) once a code mailed to the address proves it is theirs. Their other workspaces
  join it when they open them and prove it once: a click when the address is proven and the account has
  no authenticator app of its own, else that workspace's password (and app code) or a mailed code.
  Someone without a password anywhere (added without one, or through single sign-on) sets one with a
  mailed code. **Run it with a mailer**: without one, accounts are made from a workspace's password
  alone, their address unproven, and nobody can set or reset a password by mail (an administrator sets
  a workspace password; signing in with it joins the account).
* **Workspace rules stay the workspace's.** One that requires single sign-on sends its people (not its
  administrators) through it at the accounts host every time; one that requires two-factor sign-in
  asks people to turn it on for their Ferrum account first. Deactivating someone in a workspace stops
  their next sign-in there at once and their sessions with it.
* **Single sign-on.** Register `https://accounts.{base}/app/login` as the redirect URI with each
  provider: the one address for every workspace, which adding a custom domain never changes.
* **Signing out.** Of one host, from its menu (the next sign-in there asks which account); of the
  accounts host; or everywhere, from the account's browsers list, which ends every session of the
  account and of each workspace it joined, on every device.
* **Regions.** Each region runs its own cluster with its own base domain and accounts host. Set
  `FERRUM_REGION` and list the others in `FERRUM_REGIONS` (each region lists the others, by the same
  names): the accounts host sends a request for a workspace it does not have to the region that
  answers for it at `/_directory?host=` (asked with a three-second limit, remembered five minutes).
  A person's Ferrum account lives in one region, and the others trust it: their sign-in pages offer
  "your Ferrum account lives in another region", which signs in at home and comes back with the
  account's facts, taken server to server from the home's accounts host (a code bound to a PKCE
  challenge, for listed regions only). The visited region keeps a visiting account with no password:
  its password, two-factor sign-in and passkeys stay at home. Workspace data never leaves its
  region; only the account's address, name and whether it uses two-factor sign-in travel.
* **The directory** is a workspace of its own named `accounts` (made at start-up), whose people are the
  Ferrum accounts; it has no apps and nothing to administer. Start-up refuses a base domain while
  another workspace has that name.
* **Turning it off** (removing `FERRUM_BASE_DOMAIN`) leaves the hosts and accounts in place: people
  sign in on each workspace's host again with that workspace's own password, which joining never
  changed. Someone who never had one there (added without a password, then signing in with their
  Ferrum account) needs an administrator to set one.

## Sign-in security

**Two-factor sign-in.** Anyone turns it on from the account menu (Two-factor sign-in) with an
authenticator app, and keeps the ten recovery codes it shows once. Administration → Workspace →
Two-factor sign-in requires it of everyone: people without it set it up at their next sign-in.
Administration → Users → Reset 2FA is for a lost phone: it turns it off, lifts a lockout and ends
the person's sessions. Wrong codes count toward the same lockout as wrong passwords (5 in a row,
15 minutes). The secrets are sealed with `FERRUM_SECRET_KEY`.

**Single sign-on (OpenID Connect).** Register Ferrum with the identity provider as a web
application: redirect URI `https://<your host>/app/login` (Administration → Single sign-on shows
it; the server sends the provider's answer on to the sign-in page; with a base domain,
`https://accounts.{base}/app/login`), scopes `openid email profile`. Then add the provider there with its issuer URL, client ID and
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

**Emailed codes (portal people).** Customers and suppliers sign in with a six-digit code sent to
their address: it works once, for ten minutes, takes five wrong tries, and only the newest one
works; at most five are sent to a person in an hour, and the answer to "email me a code" is the
same for every address. Members never get one (they sign in with passwords, two-factor and single
sign-on as the workspace requires). Without a mailer no code can be sent.

## Routine tasks

```bash
ferrum task run_maintenance          # anchor audit trails, purge expired events, ledger partitions, queue notification digests (also runs in-process, hourly)
ferrum task sync_schema              # force the metadata → DDL sync (normally automatic at boot)
ferrum task create_tenant slug:... name:... email:... password:... [packages:...]   # core only without packages:
ferrum task deps_report [schema:<file>]   # what ties packages together; schema: writes the canonical schema to compare across a package move
ferrum task upgrade_back step:<name>      # before going back to the release before a one-time upgrade step
ferrum task seed_demo tenant:<slug>  # demo data (never in production tenants)
ferrum task import_erpnext src:<erpnext app dir> out:packages/erpnext-import
ferrum task app_make_available file:<app.fapp> [allow_unsigned:true]   # make a bundle available to every workspace (verified against FERRUM_APP_TRUSTED_KEYS)
ferrum task app_upgrade [app:<id>]   # make the newest compatible store releases of the cluster's store apps (or of one app) available
ferrum task app_revoke app:<id> [version:<v>] reason:"..."   # take a release out of service and disable it everywhere (data kept)
ferrum task app_releases [app:<id>]  # every release recorded on the cluster, with status and source
```

**Notifications by email.** A notification someone chose to have by email goes out through the
outbox right after it is made (once, even when the event is retried). Daily digests are queued by
the hourly maintenance: a person's unread digest notifications go out together once the oldest has
waited a day, at most 50 listed; claiming them and queueing the mail happen in one transaction, so
replicas never send the same ones twice. Without a mailer nothing is emailed and the choices that
need one are refused; people still see everything in the bar.

**Apps.** A release made available on the cluster is shared by every workspace (its tables are
created once, under the cluster DDL lock); each workspace then installs, upgrades or removes it
from its Apps page with an explicit consent to the permissions shown. Bundles are kept in object
storage under `apps/<id>/<version>-<digest>.fapp` so any replica can install the plugin; the
database alone rebuilds every replica's catalog. With `FERRUM_APP_UPLOADS=cluster` a workspace
administrator's upgrade from a store also upgrades the cluster's release; otherwise store upgrades
reach the cluster through `app_upgrade`, and workspaces then move to them from their Apps page.
Native apps upgrade with the server image; each workspace applies the new roles and seed data with
"Update" on its Apps page.
