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
| `FERRUM_DRIVE_MAX_FILE_MB` | no | Largest file in one request to Drive: an upload, a new version or an editor save. Default 32, at most 1024. A replica holds each such file in memory while storing it, so allow `uploads at once × this` of memory per replica. Workspaces set their own limit within it. Raise the web app's `FERRUM_MAX_UPLOAD` (nginx, default `32m`) with it, and any proxy in front |
| `FERRUM_REQUEST_TIMEOUT_MS` | no | How long one request may take, receiving its body included; default 120000 (2 minutes). Large files over slow links need more: allow the largest file divided by the slowest client's upload rate (200 MB at 10 Mbit/s is about 3 minutes). Keep the gateway's request timeout (600 s in `gateway.yaml`) above it |
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
| `FERRUM_USER_REQUESTS_PER_MINUTE` | no | Requests one user (with any of their sessions and API keys) may make per minute on each replica; beyond it they get `429` with `Retry-After`. Default 3000; `0` turns it off. It counts per replica, so the cluster-wide budget grows with the replica count |
| `FERRUM_ADDRESS_REQUESTS_PER_MINUTE` | no | Requests one client address may make per minute on each replica, counted for requests that came through the proxy (`X-Forwarded-For`); default 6000, `0` turns it off. Health probes and in-cluster calls are not counted |
| `FERRUM_PROXY_HOPS` | no | How many proxies in front append to `X-Forwarded-For`: 1 (default) for the gateway alone, 2 with a CDN or an HTTP load balancer before it. The client's address for sign-in and request limits is the entry that many from the right; set it wrong and everyone shares one address (too low) or clients choose their own (too high) |
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
4. Edit `deploy/k8s/kustomization.yaml` (image), `deployment.yaml` (`PUBLIC_URL`, bucket, region),
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

7. Optionally run `FERRUM_URL=https://<host> FERRUM_TENANT=... FERRUM_EMAIL=... FERRUM_PASSWORD=... python3 scripts/smoke.py`
   against a staging tenant (it creates a throw-away item and stock movements).
8. Sign in at `https://<host>/app/`, create the company (its chart of accounts, warehouses and,
   with `india`, GST templates are set up automatically) and a fiscal year.

Single host: `cp deploy/.env.example deploy/.env`, fill it in, then
`docker compose -f deploy/docker-compose.yml --env-file deploy/.env up -d`.

**Checking an image before it goes out.** `deploy/smoke.sh` deploys the Compose stack (database,
Ferrum, PDF renderer, web app) with freshly generated secrets. The workspace is created, seeded
and invoiced by the release being replaced (`FERRUM_PREVIOUS_IMAGE`, if set), then the server's
container is replaced with `FERRUM_IMAGE` as an upgrade does; the session, the data and the web
app's route to the new container must survive. On the new image it then checks the security and
caching headers; the web app's pages, scripts, API proxy and live-updates WebSocket; a Sales
Invoice submitted and printed as HTML and PDF with its audit chain; recovery from a database
restart; a backup, deletion of the stack with its volumes and a restore (data, session,
attachment and numbering intact); and a short load test. Everything is removed afterwards. CI
runs it on every image it builds, as an upgrade of `ghcr.io/cbferrum/ferrum:latest`, and
Ferrum-Frontend's CI runs it on every web image; images are published only when it passes.
(Ferrum-Frontend's CI pulls the server image when the `ferrum` package grants that repository
read access under *Manage Actions access*; until then it builds the server from source, about 20
minutes longer.) To run it yourself (Docker, curl, openssl, python3 with
httpx; `SMOKE_PORT`, `SMOKE_WEB_PORT` and `SMOKE_LOAD_SECONDS` override the API port 5150, the
web port 8080 and the 20-second load test):

```bash
FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:<tag> FERRUM_PREVIOUS_IMAGE=ghcr.io/cbferrum/ferrum:latest \
  FERRUM_WEB_IMAGE=ghcr.io/cbferrum/ferrum-frontend:latest deploy/smoke.sh
```

**Checking the Kubernetes deployment.** `deploy/kind/smoke.sh` deploys `deploy/k8s` to a cluster
(through `deploy/kind/kustomization.yaml`, which only swaps in local images and points
attachments and backups at an S3 emulator) and checks it as an operator would: three database
instances, the API, renderer and web app ready, the Gateway serving HTTPS with a certificate from
cert-manager, HTTP redirected, no metrics outside; no pod but the gateway's, the web app's and
monitoring's reaching the API (the network policy), metrics only with their token, the
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
docker build -t ferrum-frontend:ci https://github.com/CBFerrum/Ferrum-Frontend.git#main
kind load docker-image ferrum:ci ferrum-frontend:ci
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
| `FERRUM_OFFICE_ORIGIN` | web app | the editor as browsers reach it; the app may frame it and post to it |

**Compose.** `deploy/office.yml` adds Collabora:

    docker compose -f deploy/docker-compose.yml -f deploy/office.yml --env-file deploy/.env up -d

Set `OFFICE_PUBLIC_URL` and `OFFICE_PUBLIC_HOST` to the editor's public address.

**Kubernetes.** Run Collabora with its own Helm chart (`collabora-online`). Then:
* give it `aliasgroup1` (Ferrum's in-cluster URL) and `net.frame_ancestors` (the web app's URL);
* set the four values above on the server and the web app;
* let only the editor's pods reach `/api/site/*/drive/wopi/` (network policy).

**Things to know**
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
  `FERRUM_DRIVE_MAX_FILE_MB` (32 MB unless raised). Editor saves go straight to the server, not
  through the web app's proxy.
* **Rate limits:** the editor calls Ferrum from one address, so the public endpoints' per-address
  limit applies to all of its sessions together. Raise it for busy editors, or exempt the editor's
  address at the ingress.

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
