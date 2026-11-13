# Operations runbook

## Components

Ferrum keeps no state on its own disks or in its memory that matters beyond a request: data,
documents, plugin modules and their key-value state live in PostgreSQL, files in object storage,
sessions are signed tokens, and caches (schemas, users, plugins) are invalidated across replicas
through PostgreSQL `NOTIFY`. Any replica can be killed or added at any time.

| Component | Role | Failure behaviour |
|---|---|---|
| API replicas (`ferrum start`) | Stateless HTTP API; each also runs the outbox dispatcher, webhook sender, job runner, metadata listener and maintenance loops | Any replica may die at any time; work is claimed with `SKIP LOCKED` / advisory locks and retried by the others. Stopped (`SIGTERM`), a replica stops its background loops and live connections, answers the requests in flight (up to 20 s), then closes its database pools: within Kubernetes' 40 s grace period |
| PostgreSQL 17 (CloudNativePG) | All state: documents, ledgers, audit, outbox, plugins | Synchronous replica + automatic failover; WAL archiving for point-in-time recovery |
| Object storage (S3 or compatible) | Attachments (`FERRUM_S3_BUCKET`, `FERRUM_S3_ENDPOINT`) | Required in production; checked with a write/delete probe at boot (logged, not fatal) |

## Configuration

Production reads `config/production.yaml`, which takes everything from the environment:

| Variable | Required | Meaning |
|---|---|---|
| `DATABASE_URL` | yes | `postgres://user:pass@host:5432/db` |
| `JWT_SECRET` | yes | Base64, ≥ 32 random bytes: `openssl rand -base64 48`. Rotating it signs everyone out |
| `FERRUM_RETIRED_SECRET_KEYS` | no | Keys being rotated out, comma-separated: see *Rotating the secret key*. |
| `FERRUM_SECRET_KEY` | yes | Base64 of exactly 32 random bytes: `openssl rand -base64 32`, or that key as a key-management service's ciphertext (`vault:v1:…`, [below](#the-secret-key-from-a-key-management-service)). Seals secret fields (mail passwords, API keys) and plugins' settings (payment gateways' keys among them) with AES-256-GCM; every replica needs the same key. Back it up: values sealed with a lost key must be entered again |
| `FERRUM_KMS_TRANSIT` | no | The OpenBao or Vault Transit key that unwraps `FERRUM_SECRET_KEY` and retired keys given as ciphertext: `https://host:8200/<mount>/<key>`. Signs in with `FERRUM_KMS_TOKEN_FILE`, `BAO_TOKEN` or `VAULT_TOKEN`, or as the pod's service account (`FERRUM_KMS_KUBERNETES_ROLE`, `FERRUM_KMS_KUBERNETES_MOUNT`); `FERRUM_KMS_CA_FILE` and `FERRUM_KMS_NAMESPACE` when needed |
| `FERRUM_SECRET_KEY_COMMAND` | no | In place of `FERRUM_SECRET_KEY`: a command (run with `sh -c`, 30 seconds at most) that prints the key, such as a KMS's own CLI |
| `FERRUM_CLAMAV_ADDR` | recommended | A ClamAV daemon uploads pass through, `host:3310`. What it finds is refused and audited; with it set, a scanner that cannot answer refuses uploads too. See *Malware scanning*. |
| `FERRUM_CONNECTION_UPLOAD_MB` | no | The most a connection's call sends of stored files in one request, and the largest file it sends in chunks, in MiB (default 25). Raise it for services that take larger media in one request (Telegram 50); a description's file part may declare up to it, never past it ([BD-71](builder/decisions.md#bd-71-the-servers-upload-ceiling-and-a-file-parts-own)). Every replica the same. |
| `FERRUM_CONNECTION_HOSTS` | no | Hosts a connection may reach on a private address or over plain http, comma-separated: a service inside your own network. Others are https on public addresses only. |
| `FERRUM_CLAMAV_MAX_MB` | with a scanner | The scanner's `StreamMaxLength` in MiB (default 25). Files past it are stored unscanned. |
| `FERRUM_BOOTSTRAP_TOKEN` | no | Enables `POST /api/tenants` with header `X-Ferrum-Bootstrap-Token`; empty disables it |
| `FERRUM_OPS_TELEMETRY_DAYS` | no | Days of device measurements kept (default 30). Rollups outlive them. |
| `FERRUM_METRICS_TOKEN` | recommended | `/metrics` then requires `Authorization: Bearer <token>` |
| `FERRUM_S3_BUCKET`, `FERRUM_S3_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | multi-replica | Attachment storage on S3 (region `auto` if empty) |
| `FERRUM_S3_ENDPOINT` | no | Any S3-compatible store: MinIO, Cloudflare R2, Wasabi, Backblaze B2, DigitalOcean Spaces, GCS interoperability |
| `FERRUM_DRIVE_MAX_FILE_MB` | no | Largest file in one request to Drive: an upload, a new version or an editor save. Default 32, at most 1024. A replica holds each such file in memory while storing it, so allow `uploads at once × this` of memory per replica. Workspaces set their own limit within it. Raise any proxy's body limit in front with it |
| `FERRUM_REQUEST_TIMEOUT_MS` | no | How long one request may take, receiving its body included; default 120000 (2 minutes). Large files over slow links need more: allow the largest file divided by the slowest client's upload rate (200 MB at 10 Mbit/s is about 3 minutes). Keep the gateway's request timeout (600 s in `gateway.yaml`) above it |
| `FERRUM_DIRECT_UPLOAD_MAX_BYTES` | no | Largest file browsers upload straight to the bucket, or in parts through the server (default 2 GiB). Straight to the bucket needs the bucket CORS rule below |
| `FERRUM_PDF_URL` | no | A Gotenberg-compatible renderer (`gotenberg/gotenberg:8`, included in both deployments as `pdf` / `ferrum-pdf`) for PDFs of print formats; empty turns PDFs off. Run it with JavaScript off and an allow-list of `^(data:\|file:///tmp/).*` so templates cannot make it fetch anything; print formats then use inline (data:) images |
| `FERRUM_REQUIRE_OBJECT_STORAGE` | no | Production default `true`: refuse to start without object storage, so no replica keeps files locally. `false` only for single-server installs |
| `FERRUM_FILES_DIR` | no | Local attachment directory (default `/var/lib/ferrum/files`) |
| `FERRUM_DB_MAX_CONNECTIONS` | no | Pool size per replica (default 30). Below 10, requests wait behind the replica's own job workers and event loops, which can hold seven at once; the server warns as it starts, and `ferrum task doctor` says so |
| `FERRUM_RUN_BACKGROUND` | no | `false` turns a replica into API-only |
| `FERRUM_MIGRATE_AT_START` | no | Default `true`: the server migrates the database, syncs the schema and runs the upgrade steps as it starts. `false`: it changes no schema, checks that `ferrum task migrate` has brought the database to this build, and refuses to start, saying so, when it has not. The task does everything either way, and several at once are safe |
| `FERRUM_UPKEEP_CONCURRENCY` | no | How many apps' periodic upkeep one replica runs at once (default 2). Each app's upkeep has a lock of its own across replicas, so replicas share the apps between them; each running app holds one database connection |
| `PUBLIC_URL` | recommended | The address people open, scheme and port included (`https://erp.example.com`). Apps' screens name it in their Content Security Policy, which Safari needs when a proxy in front rewrites the host, and links mailed to people outside the workspace (Drive guests) are built on it, as are the short-lived addresses services pull files from (`/files/pull/…`, BD-64; without it such calls are refused) |
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
| `FERRUM_TENANT_REQUESTS` | no | One workspace's API requests that run at once on each replica. The rest wait their turn, and one that waits five seconds is refused with `429`. A response that waited says how long in `X-Ferrum-Waited-Ms`. Default half of `FERRUM_DB_MAX_CONNECTIONS`, at least 2, so a workspace that floods the server holds at most half its database connections and everyone else has the others; `0` turns it off |
| `FERRUM_PROXY_HOPS` | no | How many proxies in front append to `X-Forwarded-For`: 1 (the default) for the Kubernetes gateway alone or your own TLS proxy, 0 when browsers reach the server directly (Compose sets it; set 1 there behind your proxy), 2 with a CDN or an HTTP load balancer before the proxy. The client's address for sign-in and request limits is the entry that many from the right; set it wrong and everyone shares one address (too low) or clients choose their own (too high) |
| `FERRUM_SSO_ALLOW_PRIVATE` | no | `true` lets single sign-on reach an identity provider on a private address or over plain http (one inside your own network, such as an in-cluster Keycloak); default `false` |
| `FERRUM_APP_ALLOW_UNSIGNED` | no | `true` accepts uploads no trusted key signed (development default); production default `false` |
| `FERRUM_SESSION_DAYS` | no | How long a browser stays signed in with the session cookie (default 14); each browser is a session of its own that people and administrators sign out one by one |
| `FERRUM_BASE_DOMAIN` | no | Shapes B and C (below, [Many workspaces](#many-workspaces-subdomains-and-ferrum-accounts)): the domain under which each workspace is `{slug}.{base}`, people sign in at `accounts.{base}` and outside callers use `hooks.{base}`. Empty: one workspace (`FERRUM_DEFAULT_WORKSPACE`) or a shared host |
| `FERRUM_PLANS` | no | The operator's plans, JSON: `{"free": {"people": 5, "storage_bytes": 1073741824}, "team": {"people": 50}}`. Each sets quotas a workspace on it keeps to; see [Plans and sign-up](#plans-and-sign-up) |
| `FERRUM_SIGNUP` | no | `open` lets people make workspaces of their own at `accounts.{base}/signup`; default `closed`. Needs `FERRUM_BASE_DOMAIN` and mail |
| `FERRUM_SIGNUP_DOMAINS`, `FERRUM_SIGNUP_PACKAGES`, `FERRUM_SIGNUP_PLAN`, `FERRUM_SIGNUP_PER_ADDRESS` | with sign-up | The address domains sign-ups come from (comma-separated; empty: any), what a new workspace installs besides the core, the plan it starts on (one of `FERRUM_PLANS`), and how many workspaces one address may make (default 3) |
| `FERRUM_REGION`, `FERRUM_REGIONS` | no | This cluster's region (`eu`), and the other regions' accounts hosts, `name=https://accounts.example.eu,…`: the sign-in page offers them, and a workspace not found here is looked up there |
| `FERRUM_DEFAULT_WORKSPACE` | no | The slug of the one workspace this server serves on every host (a single-workspace install): sign-in then asks for no workspace. Empty: a host registered for a workspace leads to it, and elsewhere the workspace is chosen at sign-in |
| `FERRUM_UI_DIR` | no | Interfaces registered at start-up from a directory with one directory of built files per app id (`<dir>/<app>/index.html`), for apps whose crate carries none: an interface compiled into an app's crate is the one served, and a copy of it here is skipped with a warning. The image sets it to `/app/ui`, which holds the shell's and Studio's (built from Ferrum-Frontend at the commit the Dockerfile pins); point it elsewhere to run others |
| `FERRUM_OFFICE_ORIGIN` | with online editing | The editor server as browsers reach it (`https://office.example.com`), when that is not `FERRUM_OFFICE_URL`: Drive's pages frame it and post to it, and their Content-Security-Policy names it (the Compose stack sets it from `OFFICE_PUBLIC_URL`, the Helm chart from `office.host`) |
| `FERRUM_APPS_BASE` | no | A domain under which isolated apps get origins of their own, `{app}--{workspace}.{base}` (`example-apps.net`); see "Apps with interfaces of their own". Empty (the default): isolated apps run in sandboxed frames. `localhost` works in development |
| `FERRUM_UI_SOURCE` | no | Development: a server listing built interfaces (`index.json`, as Ferrum-Frontend's `npm run build:apps` writes `dist-apps/`), fetched at start-up and every ten minutes, files checked against their digests |

**Database role.** Ferrum must connect as an ordinary role that owns its database, never as a
superuser or a role with `BYPASSRLS`: those bypass PostgreSQL row-level security and with it tenant
isolation, so the server refuses to start with them. CloudNativePG's `ferrum` application user and
the Compose setup (`deploy/postgres-init`) are already set up this way. The role needs to create the
trusted extensions `btree_gist`, `citext` and `pg_trgm`, which a database owner can do.

Each replica opens up to `FERRUM_DB_MAX_CONNECTIONS` connections plus 4 for short side writes
(document numbering). Give the role a connection limit at least 20 below the database's
`max_connections`, so that however far things scale there are connections left for an administrator,
replication and a migration: PostgreSQL enforces that one, and `ferrum task doctor` warns without it.

| Who | Connections at the most, in the manifests |
|---|---|
| API, 10 replicas × (30 + 4) | 340 |
| Cloud's controller, 2 replicas × (6 + 4), if deployed | 20 |
| The role's limit | 370 |
| Left for administration and replication | 30 |

`python3 scripts/check_manifests.py` fails when those stop adding up, and when a workload sets `replicas`
under an autoscaler: every apply would put the count back. The Helm chart's migrate Job is a process of its own
with as many connections as a replica; the check counts it (see *Deployment shapes*).
Past the limit a request waits for its pool and answers 503; nothing queues without bound.

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
4. Edit `deploy/k8s/kustomization.yaml` (the images' tag: a commit's, see *Images* below; until it is set the pods fail to pull), `deployment.yaml` (`PUBLIC_URL`, bucket, region),
   `gateway.yaml` (host; `gatewayClassName` if not Envoy Gateway's `eg`) and `database.yaml` (the
   `ObjectStore`'s backup bucket, sizes). `envoy-proxy.yaml` runs two of Envoy Gateway's proxies on
   different nodes and keeps the client's address (`externalTrafficPolicy: Local`), which the
   per-address limits count. With another gateway implementation, remove that file and the
   Gateway's `infrastructure.parametersRef`, give its proxies the same properties, and allow the
   namespace they run in (Envoy Gateway: `envoy-gateway-system`) in `networkpolicy.yaml`.
   Create the `letsencrypt` ClusterIssuer the Gateway names, with an ACME HTTP-01 solver of type
   `gatewayHTTPRoute` whose `parentRefs` is the Gateway `ferrum` in namespace `ferrum` (or use a DNS-01
   solver). With a CDN or HTTP load balancer in front of the gateway, set `FERRUM_PROXY_HOPS=2`.
5. `kubectl apply -k deploy/k8s`, and `kubectl apply -k deploy/k8s/office` for the online editor (Word, Excel
   and PowerPoint files in Drive; on by default, in a namespace of its own: replace `office.example.com` in
   `deploy/k8s/office/office.yaml`, `gateway.yaml` and `deployment.yaml` with its host, see *Online editing*), and
   wait for `kubectl -n ferrum get pods` to show the database cluster healthy and the API pods ready
   (`/api/ready` checks the database).
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

**Installing with the Helm chart.** `deploy/helm/ferrum` renders what `deploy/k8s` renders, so one command
replaces step 5 (steps 1 to 3 and 6 to 8 are the same):

```bash
helm install ferrum deploy/helm/ferrum --namespace ferrum --create-namespace \
  --set image.digest=sha256:<digest> \
  --set publicUrl=https://<host> --set gateway.hostname=<host> \
  --set storage.bucket=<files bucket> --set database.backup.destinationPath=s3://<backup bucket>/ferrum
```

* *The secret.* The chart creates none. It expects the Secret `ferrum-secrets` (another name: `secrets.existingSecret`)
  with the keys of `deploy/k8s/secret.example.yaml`, and the backups' Secret `ferrum-backup-s3`.
* *The image.* Set `image.digest` (a digest wins) or `image.tag` (a commit's full SHA). With neither, the chart refuses to
  render; there is no default and never `latest`. The same holds for the optional parts' images.
* *Settings.* `values.yaml` lists them with the environment variable each becomes; `extraEnv` adds any other. With
  `database.enabled=false` the chart makes no database: name the Secret holding your connection URI in `database.urlSecret`.
* *The online editor.* `office.enabled` (on by default): Collabora in the namespace `office.namespace`, on the host
  `office.host` with its own certificate, with its limits, probes and network policy; see *Online editing*. The image
  is an exact tag (`office.image`; a digest wins), pinned like the PDF renderer's.
* *Optional parts.* `editors.enabled` (Browser editors; set `editors.host` and its image) and `metricsEngine.enabled`
  (create what *Metrics' engine for external sources* lists first) switch on the same components as in `deploy/k8s`.
  The PDF renderer, autoscaler, budget, network policy, `ServiceMonitor`, Gateway, Envoy proxies and database are on by
  default, each behind its own `enabled`.
* *Migrations.* Off by default: every pod migrates itself as it starts. `migrate.enabled` adds a Helm hook Job that runs
  `ferrum task migrate` before an install or upgrade (`migrate.args`) and sets `FERRUM_MIGRATE_AT_START=false` on the
  server (`migrate.serverEnv`), so the pods only check that the database is ready and refuse to start, naming the
  command, when it is not. On a first install with `database.enabled` the database does not exist yet when hooks run,
  so that one install has no Job and the pods migrate themselves; every `helm upgrade` after it uses the Job. (Tools
  that render with `helm template` and never mark an upgrade, such as some GitOps setups, then never get the Job:
  the pods keep migrating themselves, which is safe.)

`bash deploy/helm/test.sh` (CI runs it) fails when the chart and `deploy/k8s` differ in resources, environment variables,
ports, probes, security context or resources.

**Images.** For each commit on `main` whose images pass every check, CI publishes
`ghcr.io/cbferrum/ferrum` and `ghcr.io/cbferrum/ferrum-editors` tagged with the commit's full SHA.
What is published is the image that was tested, promoted by digest, never a new build of it. The
server's image carries an SBOM and build provenance, and the editors' image its provenance: check
them with `gh attestation verify oci://<image>@<digest> --repo CBFerrum/Ferrum`. `latest` moves to a
release only after it also passes the Kubernetes qualification. The server's image answers every page: it carries the shell's and Studio's interfaces,
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

## A site without internet

Dedicated and on-premises installations run the same images as any other; a site that cannot reach
the internet gets them, and everything they run beside, in a bundle made on a connected machine and
carried across. `deploy/offline/` makes and loads it (needs python3; skopeo, which keeps every
image's digest, or docker on one host).

**On a connected machine**, from a checkout at the release's commit (the images' tag, *Images*
above), signed in to `ghcr.io` with a token that may read packages while they are private:

```bash
python3 -m pip install pyyaml
deploy/offline/bundle.py <commit> --with editors,mail,clamav            # one host (Compose)
deploy/offline/bundle.py <commit> --kubernetes --with editors,clamav    # Kubernetes
tar -cf ferrum-offline-<commit>.tar ferrum-offline-<commit>/
```

The bundle holds every image (the server's at that commit, the database's, the PDF renderer's, the online editor's, and
what `--with` adds: `editors`, `mail`, `meet`, `clamav`; the editor, Collabora, is in the stack); with `--kubernetes`, the operators'
manifests at the versions `deploy/kind/install.sh` names and every image they name, including the ones
named only in their settings or in a Secret, and the Envoy proxy Envoy Gateway starts, which only its
code names; this checkout's `deploy/` with these tools, `deploy/k8s` already naming the release;
`bundle.json` (each image with its digest, each manifest with where it came from) and `SHA256SUMS`.
Images named by a moving tag are copied as they are that day: pin them (`COLLABORA_TAG`,
`STALWART_TAG`, `CLAMAV_TAG`) to know what was carried. Images pinned by digest are copied whole, every
platform, so the digest still answers.

**At the site**, unpack it and check it first; anything changed, missing or added refuses it:

```bash
ferrum-offline-<commit>/deploy/offline/load.py --check
```

* *One host*: `deploy/offline/load.py` puts the images into Docker under their own names and writes
  `release.env`; fill in `deploy/.env` (*Configuration*) and start it with
  `docker compose -f deploy/docker-compose.yml --env-file deploy/.env --env-file release.env up -d`.
* *Kubernetes*: put the images in the site's registry (any OCI registry: Harbor, Zot, CNCF
  Distribution) with `deploy/offline/load.py --registry registry.site:5000` (`--insecure` for plain
  http or a certificate the nodes do not trust). Each image goes to
  `<registry>/<the registry it came from>/<its path>`, so nothing is renamed: `containerd/certs.d/`
  in the bundle holds a `hosts.toml` for each registry they came from, which sends that registry's
  pulls to the site's; copy it to `/etc/containerd/certs.d/` on every node (containerd reads it with
  `config_path = "/etc/containerd/certs.d"` in its CRI registry settings). Then, with the site's
  certificate authority in the secret `site-ca` (`kubectl create namespace cert-manager`, then
  `kubectl -n cert-manager create secret tls site-ca --cert=ca.crt --key=ca.key`),
  `deploy/offline/install.sh` installs the operators from the bundle (`--kubelet-insecure-tls` when the
  kubelets' certificates are self-signed), and the Gateway's certificate is issued by the site's
  authority, which browsers must trust. Go on from step 2 of *First deployment*, with buckets on
  the site's own S3-compatible storage.

What needs the internet, and what a site without it does instead:

| | Without the internet |
|---|---|
| Apps from a store | `FERRUM_APP_STORES` empty: apps compiled in, and bundles signed by a key in `FERRUM_APP_TRUSTED_KEYS` made available with `ferrum task app_make_available` |
| Certificates | The site's own authority (above), not ACME |
| Mail | The site's relay, or hosted mail (`deploy/mail.yml`) inside the site |
| Malware signatures | ClamAV's `freshclam` downloads them: mirror the database on a connected machine with `cvdupdate`, carry it, and point `freshclam` at it (`PrivateMirror`), or scanning goes on with the signatures it has |
| The assistant | A model served inside the site: `FERRUM_ASSISTANT_PROTOCOL=openai` and `FERRUM_ASSISTANT_URL` at an OpenAI-compatible server (*The assistant*) |
| Notifications on devices | Browsers' push services are on the internet: notifications stay in the apps |
| Webhooks, connectors, e-invoice and tax portals, payment providers, maps, single sign-on with a provider outside | Not reached; what the site runs inside works (`FERRUM_SSO_ALLOW_PRIVATE=true` for its own identity provider) |
| Other regions and Ferrum accounts across them | One region |
| Time | The site's own NTP: sessions, codes and certificates depend on clocks |

**Upgrades** come the same way: a bundle of the next release, loaded as above, then *Upgrades* below
(the database's migrations run as the new server starts). Keep the bundle of the release that runs
until the next one has proved itself, to go back to. **Backups** go to the site's own object
storage (*Backups and restore*); copies leave it by whatever route the site's policy allows.

`deploy/offline/test_offline.py` checks what a bundle carries and what loading does against
stand-ins for skopeo, docker and kubectl; CI runs it. No bundle has yet been carried into a site, and
the operators' install from one has not run on a cluster without internet.

## Upgrades

* Before a release is tagged, two checks must pass. `scripts/release_evidence.py qualified` says whether each
  qualification (docs/logic/evidence/qualification.yaml) ran at this very build; when Ferrum's code or any pin has
  changed since, it names what changed and fails until the qualification's scripts are run again and
  `--stamp <id>` records the build they ran at. `scripts/licenses.py check` fails on any image a deploy file names
  that is not an engine approved in docs/registry/engines.yaml, its licence read and its terms stated.
* Schema changes are forward-only migrations plus a metadata → DDL sync, both run at boot under
  advisory locks, so rolling several replicas at once is safe. Where migrating is a step of its own
  (`FERRUM_MIGRATE_AT_START=false`, as the Helm chart's migrate Job sets it), `ferrum task migrate` runs them, with the
  one-time steps below, and the servers only check. Upgrades only add tables, columns
  and indexes; nothing is dropped automatically (orphaned columns are reported in the logs).
* Indexes the sync adds to entity tables are built concurrently, so writes carry on, and a build that
  failed is dropped and built again at the next start. The release that gave lists an index in their
  default order adds one per entity: on large tables the first start after it takes a while.
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

  `attach-files` comes with the release in which a file tied to no record is served only to its
  uploader and administrators, and saving a record ties the files its `attach` fields name to it
  ([api](api.md#files)). Before that release a file uploaded for a record not yet saved stayed tied to
  no record. The step ties each such file to the record whose `attach` field names it (a line's to
  the record owning the line; custom entities' too), in batches of 1000, so that record's readers keep
  it. A file several records name goes to the first made: the others' readers no longer download it,
  and the log counts such files per workspace. A file no record names stays its uploader's. It runs
  before the new release serves, in each way of running it:
  * one server migrating as it starts (`FERRUM_MIGRATE_AT_START=true`, the default): migrations, the
    schema sync, then the steps, before it takes requests. A step that fails is logged and the server
    serves anyway (the next start tries again): check the log for `attach-files` after upgrading;
  * `ferrum task migrate`, then a rolling restart: the task runs the step; the new servers refuse to
    start until it has;
  * the Helm chart with `migrate.enabled`: the pre-upgrade Job runs `ferrum task migrate` before the
    Deployment changes; the new pods only check.

  In the last two, and with several replicas in the first, servers of the release before keep taking
  requests until they are replaced, and a record they save meanwhile leaves its new file tied to no
  record. To tie those too, once the rollout is over run `ferrum task upgrade_back step:attach-files`
  and then `ferrum task migrate` (the step is safe to run again and alongside servers taking requests).

  To go back to a release before such a step, run `ferrum task upgrade_back step:<name>`
  (`core-split`) with every replica of the newer release stopped, then start the older image. The
  task takes out the packages the step installed and keeps the data.
* Data a release moves after its schema has grown is a package's backfill (`backfills` in its
  manifest: a table or an entity, which rows still `need` it, what it `set`s, how many a batch).
  The hourly upkeep starts each one in every workspace that has the package, as a job of its own:
  a batch at a time, each in its own transaction with the last row it reached, so a restart, a
  crash or another replica carries on from there, and the workspace keeps working meanwhile. An
  unfinished one is queued again at each upkeep. When done it counts the rows that still need it:
  `GET /api/admin/backfills` reports, per backfill, its state, the rows it changed, those it could
  not, and those that need it now; `POST /api/admin/backfills/{package}/{name}/rerun` runs a finished
  one again (after the rows it could not change were put right). Contract (drop what the backfill
  replaced) only once every workspace reports none remaining.

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
  docker compose exec -T ferrum tar -C /var/lib/ferrum/files --warning=no-file-changed -czf - . > files-$(date +%F).tgz
  ```

  Take the attachments after the database, so that they include everything the dump refers to,
  and every erasure journalled since (`privacy-journal/`, which `ferrum task reapply_erasures`
  replays after a restore). The server keeps writing while tar reads. An exit status of 1 means a
  file changed as it was read, and the archive still holds it as it was then; 2 means the archive
  failed.

  To restore onto a new host, start the database alone (`docker compose up -d db`), wait until
  `docker compose exec db pg_isready -h 127.0.0.1` succeeds, then:

  ```bash
  docker compose exec -T db pg_restore -U postgres -d ferrum --clean --if-exists --exit-on-error < ferrum.dump
  docker compose run --rm --no-deps -T --entrypoint tar ferrum -C /var/lib/ferrum/files -xzf - < files.tgz
  docker compose up -d
  ```

  Restoring in place, stop the server and restore into an empty database, as on a new host:
  `pg_restore --clean` over the old one cannot drop a partitioned table's partitions one by one
  (the journals and measurements are partitioned), and stops partway with the database half
  dropped.

  ```bash
  docker compose stop ferrum
  docker compose exec -T db dropdb -U postgres --force ferrum
  docker compose exec -T db createdb -U postgres -O ferrum ferrum
  docker compose exec -T db pg_restore -U postgres -d ferrum --exit-on-error < ferrum.dump
  docker compose up -d ferrum
  ```

  Then run `ferrum task reapply_erasures` (After restoring a backup, below) before anyone uses it.
* Test a restore at least quarterly. After restoring, run the audit verification below.

## Regions

Where a region can fail over, give each one the same witness: a small PostgreSQL neither depends on for
anything else, in a third place.

| Setting | What it is |
|---|---|
| `FERRUM_AUTHORITY_WITNESS_URL` | The witness. Unset, this server writes without asking anyone, which is right for one region |
| `FERRUM_REGION` | This region's name |
| `FERRUM_AUTHORITY_TTL_SECS` | How long it holds the right to write between renewals. 30 by default |

A region writes only while it knows it holds that right. It stops itself a quarter of the way before its
time runs out, measured from before it last asked, so by the time anyone else can be given it, this region
has stopped. Stopped means every transaction is read-only: reading works, writing answers 503, and work
that reaches outside does not run. Losing the witness therefore stops writes everywhere until it is back.
That is the choice being made: no writes over two writers.

A region that lost the right does not take it back by itself, even if nobody else holds it, because its data
is behind. Rebuild it from the region that took over, then:

```bash
ferrum task accept_authority note:"rebuilt from eu-west at 14:20"
```

This has been tested with real connections to a real witness, and two databases on one machine. It has not
been run across real regions, and the database's own replication and promotion are yours to run.

## After restoring a backup

When a server starts on a database that is not the one it knows, or one on a new timeline, it holds
everything that reaches outside: mail, webhooks, jobs, what apps have in hand, and commands to equipment.
What was waiting is set aside, because some of it may have happened after the backup was taken and nothing
here can tell. Mail and webhooks go to the dead events and deliveries, jobs fail with a reason, and operations
and commands become unknown. `ferrum task doctor` says so.

A failover to a synchronous replica is also a new timeline, and loses nothing. Set
`FERRUM_HOLD_ON_TIMELINE=false` if that is the only way a timeline changes here. Then an in-place
point-in-time restore is not noticed, so run the steps below by hand after one.

A database the Helm chart starts from another one (`database.bootstrap.mode`, "Moving from one shape to
another") is held the same way: `import` makes a database this server never knew, and `recovery` and `copy`
start on a new timeline. A `copy` keeps the database's identity, so with `FERRUM_HOLD_ON_TIMELINE=false` it
is not held, and neither is a `recovery`, whose base backup has the same identity as the cluster it came
from. Release the hold with `ferrum task release_restore` once you know nothing will be sent twice.

A database restored from a backup brings back whatever was erased since it was taken, and lets back in
whoever left since, so do this before people, integrations or the assistant reach it:

```bash
ferrum task reapply_erasures
```

Each erasure is journalled in object storage (`privacy-journal/`), by ids and field names and never
by value, so the journal survives a database restore and holds nothing it erased. Keep that prefix out
of any lifecycle rule that would expire it sooner than your oldest backup. If object storage was
restored too, journals written after that backup are gone: take the erasures since then from your
request records and run them again.

The same task applies again every change of someone's access since the backup: who left or was turned
off is out again, with every session, key and grant ended, and who came back is in. Each change is
journalled in object storage (`access-journal/`) by the person's id only, and only each person's last
word counts. Keep that prefix as long as `privacy-journal/`; if it is gone, take departures since the
backup from your identity provider.

## Rotating the secret key

`FERRUM_SECRET_KEY` seals secret fields, authenticator secrets, single sign-on client secrets,
webhook signing secrets, plugins' settings and the Web Push key. To replace it, on a schedule or
because the old key may have been seen:

1. Generate a new key: `openssl rand -base64 32`.
2. Give **every** replica both: add the new key to `FERRUM_RETIRED_SECRET_KEYS` and restart them all.
   Nothing changes yet, but each replica can now open what the new key will seal, so a rolling
   restart in the next step cannot leave one that cannot read a value.
3. Set `FERRUM_SECRET_KEY` to the new key and `FERRUM_RETIRED_SECRET_KEYS` to the old one, and restart.
   Each replica moves stored values to the new key as it starts. It is safe side by side with
   traffic and with other replicas: a value changed meanwhile is left for the next start.
4. Check the log. `secrets remain that no configured key opens` means some values were sealed with
   a key you have not configured: keep the retired keys until you know why.
5. Remove the old key from `FERRUM_RETIRED_SECRET_KEYS` and restart. Back up the new key.

A backup taken before the rotation still needs the old key, so keep it as long as those backups.

## The secret key from a key-management service

The key need not sit in the configuration in the clear. Two ways, both read once at start-up, the
key then living only in the server's memory:

* **OpenBao or Vault Transit.** Make a Transit key (`bao secrets enable transit`,
  `bao write -f transit/keys/ferrum`), give Ferrum a policy allowing `update` on
  `transit/encrypt/ferrum` and `transit/decrypt/ferrum`, and set `FERRUM_KMS_TRANSIT` to
  `https://bao.internal:8200/transit/ferrum`. With the key still configured in the clear,
  `ferrum task kms_wrap` prints `FERRUM_SECRET_KEY=vault:v1:…` (and `FERRUM_RETIRED_SECRET_KEYS`, if
  any): configure those in its place and restart. It never prints a key. When the service rotates its
  Transit key, what it wrapped before still opens; run `kms_wrap` again to move to the newest version.
  In Kubernetes, sign in as the pod (`FERRUM_KMS_KUBERNETES_ROLE`, the Kubernetes auth method) or
  through a Vault Agent's token file (`FERRUM_KMS_TOKEN_FILE`).
* **A command.** `FERRUM_SECRET_KEY_COMMAND` runs once at start and must print the key, as
  PostgreSQL's `ssl_passphrase_command` does. Any KMS's own tool works, for example
  `aws kms decrypt --ciphertext-blob fileb:///etc/ferrum/key.enc --query Plaintext --output text`
  (the file holding `aws kms encrypt`'s output for the key's 32 bytes). What it prints on stderr is
  not logged.

A server that cannot get its key does not start, and says why (`ferrum task doctor` says where the
key comes from). The service is then part of starting: run it, and back it up, accordingly. Keep a
copy of the key itself where your recovery plan says: without the service and without that copy,
what the key sealed is lost.

**If a key is lost**, what it sealed cannot be recovered: nothing else holds those values. Secret
fields answer that they were sealed with another key. People with an authenticator app cannot use
it, and an administrator resets their two-factor sign-in. Single sign-on fails until the client
secret is entered again. Plugins start with those settings missing, so re-enter them. Webhooks fail
rather than go out unsigned, so make new ones. The Web Push key is replaced, and devices subscribe
again. Passwords, sessions and business data are not affected. Restoring a backup without its key
leaves the same state.

## Durable processes

Customers' solutions may hold processes that wait for people, events and deadlines (the Full Process
profile). They run on a [Temporal](https://temporal.io) server (MIT) beside Ferrum: set `TEMPORAL_ADDRESS`
(and `TEMPORAL_NAMESPACE`, created beforehand) on the replicas that run background work. Without it, a
solution with a process is refused and everything else runs as before. What Temporal stores is sealed with
`FERRUM_SECRET_KEY`; after rotating it, keep the old key in `FERRUM_RETIRED_SECRET_KEYS` until the runs
started before have finished. Settings and behaviour: [logic/processes.md](logic/processes.md).

**Accounts solutions act as.** A run no person's act started acts as its solution's own service account,
`solution.{id}@services.ferrum.invalid`, made at activation with exactly the rights its release was reviewed to
use and turned off when the solution stops. It takes no seat. Do not give it more rights by hand: each activation
puts back the reviewed closure, and a run that needs more needs a reviewed release that asks for it.

**Running Temporal.** With Docker Compose, `deploy/temporal.yml` adds Temporal Server 1.32.2 (the version
qualified on PostgreSQL 17) keeping its histories in the stack's PostgreSQL, databases `temporal` and
`temporal_visibility`, sets up their schemas and the namespace, and points Ferrum at it:

```bash
docker compose -f deploy/docker-compose.yml -f deploy/temporal.yml --env-file deploy/.env up -d
```

Nothing is published to the host. Back up the two Temporal databases with Ferrum's: a restore brings back
both, and Ferrum then holds (below).

**Watching.** `/metrics` carries `ferrum_logic_runs{status}` (starting, running, needs_attention),
`ferrum_logic_start_wait_seconds` (how long the oldest accepted run has waited for Temporal to start it),
`ferrum_logic_tasks_open`, `ferrum_logic_tasks_overdue` and `ferrum_logic_recovery_open`. Alert when the
start wait passes a few minutes (Temporal or the worker is away: runs are accepted and wait, nothing is
lost), and when recovery items stay open (a person must look: Studio, Tasks).

**After a restore.** Ferrum holds after it notices a restore (see "After restoring a backup"): besides jobs
and the outbox, a process's steps that would change something wait and are tried again, while reads and
people's tasks go on. Release the hold once what came back is reconciled; the waiting steps then run, each
under the same receipt, so none happens twice. Releasing it also asks every open run where it stands. A run
whose history in Temporal is ahead of the restored database (Temporal's store backed up later than
PostgreSQL) then names each effect it did that this database has no receipt for, as an attention item in
Studio, Tasks: check whether it happened and put right what the restore lost; nothing is done again by
itself. `scripts/restore_drill.py` rehearses all of this into a new, isolated environment.

**How much the worker takes on.** The process worker runs at most half the database pool's connections in
activities at once, keeps a quarter of those for bookkeeping (people's tasks, reports, deadlines), and lets
one workspace hold at most a quarter of the rest as effects; a step over its workspace's share waits its
turn and is tried again under the same receipt, so one busy workspace cannot hold back the others or
anyone's recovery. Set `FERRUM_PROCESS_STEPS` (activities at once) and `FERRUM_PROCESS_STEPS_PER_WORKSPACE`
to change them, and size the pool (`FERRUM_DB_MAX_CONNECTIONS` or the database settings) with replicas in
mind. `/metrics` shows `ferrum_logic_steps_in_hand`, `ferrum_logic_steps_workspaces`,
`ferrum_logic_steps_deferred_total` and the limits; deferrals climbing steadily mean a workspace is asking
for more than its share for a long time. `scripts/logic_load.py` measures a logic workload and this
fairness on a running build.

**Upgrading with processes open.** A new Ferrum build must read open runs as the old one did. Before it
takes over, export the histories of open runs and replay them on it:

```bash
TEMPORAL_ADDRESS=http://temporal:7233 ferrum task logic_replay
```

It fetches the history of every run that has not finished from Temporal and replays each on the new build
(histories saved by the SDK can be given instead, `files:a.json,b.json`; the Temporal CLI's own JSON is not
that format).

A history that does not replay means the new build would decide those runs differently: keep the old build
running them, and say so in the release notes. Replay needs the same `FERRUM_SECRET_KEY` (and retired keys)
as the runs were sealed with.

**From automations.** Studio's `POST /api/app/studio/solutions/import/automations` (administrators) turns each
automation into a draft Solution: the same event, the condition as written, the actions as steps through
capabilities, a run per event as the automation ran, and where it came from. It is a draft: someone finishes
what did not map, keeps it as a release, and another administrator reviews and consents to what it may change.
A scheduled automation is not imported (a solution starts on a record's event); it keeps running as it is.
Activating that release turns the automation off in the same step, and from then on each of its events is
claimed once, by the solution or by the automation's runner, whichever comes to it first: never run by both,
never by neither, however the events and the activation interleave. While the solution runs, the automation
cannot be turned back on; to go back, deactivate the solution first.

**Comparing before it runs.** On a release's page in Studio (or `POST /api/app/studio/solutions/shadow`),
recent work is replayed against the release and against what runs now: the last runs of its solution, with
the answers, decisions and events they met, or, for a solution imported from an automation that has not taken
over yet, the automation's recent events and what it did. Each case says what differs. Nothing runs, changes or
is kept, so it is safe on production data; a step the candidate adds has no recorded answer and says so.

**Going back.** Going back to an earlier release is activating it again: a new generation, new runs start under
it, runs started under the later one go on under theirs, and nothing any run did is undone. The activation
says which release it replaced and how many runs stay on another; each run records the generation it started
under.

**A capability out of service.** When an app's release is revoked, an app is taken out of a workspace, or the
version a release is bound to is replaced, a step that would use one of its capabilities neither runs nor fails:
it waits, an attention item tells administrators why (Studio, Tasks), and it goes on by itself, under its own
receipt, once the capability is back. To stop instead, cancel the run. Nothing similarly named stands in.

**Moving a waiting run.** A run keeps the release it started under. To move one that waits (at a task, an event,
a delay or a stop) onto the release that runs now, an administrator plans it on the run's page with a mapping of
each step it waits at (`POST /runs/{id}/migrate/plan`), and another administrator approves exactly that plan
(`POST /runs/{id}/migrate`). The runtime carries it out on the run as it holds it, checked where the records
are: a decision or an event that came meanwhile wins, and the move is refused and recorded. A run inside a loop,
parallel branches or a subprocess, or with an effect in hand, does not move.

**Packs between workspaces.** A release downloads as a pack (`GET /solutions/releases/{id}/pack`): its source,
digests, the capabilities, events and calendars it needs, and its review decisions without names. A pack never
carries a connection or a credential. Taken in elsewhere (`POST /solutions/import/pack`) it becomes a draft, with
what must be mapped there; a pack changed on the way is refused.

## Metrics' engine for external sources

A metric whose numbers live in a data lake's CSV or Parquet files is read through Metrics' engine. The engine
is a service of its own (`ferrum-metrics-engine`, built from the Metrics repository's `engine/Dockerfile` and
published from its main as `ghcr.io/cbferrum/ferrum-metrics-engine`). It
plans the metric's query through the metric's Wren semantic model with Wren's engine and runs it on DataFusion,
so a heavy query is bounded there and cannot take the server down. The server never links it.

```bash
docker compose -f deploy/docker-compose.yml -f deploy/metrics-engine.yml --env-file deploy/.env up -d
```

- **Configuration.** `deploy/.env` holds `FERRUM_METRICS_ENGINE_TOKEN`, at least 32 characters; Ferrum and the
  engine hold the same one. `deploy/metrics-engine.json` lists the datasets: each has a name, its workspace by
  slug or id, a format, and a path under `/data`. It also sets the limits: rows, time, memory (nothing spills),
  and queries at once. The datasets directory (`FERRUM_METRICS_DATASETS`) is mounted read-only. Nothing is
  published to the host. Compose builds the image from a Metrics checkout (`FERRUM_METRICS_REPO`) unless
  `FERRUM_METRICS_ENGINE_IMAGE` names the published one.
- **On Kubernetes,** list the `metrics-engine` component in `deploy/k8s/kustomization.yaml`. It runs two
  replicas, either one answering, read-only and unprivileged, with a disruption budget; only the API may call
  them, and they call nothing. It points Ferrum at them and lets Ferrum's network policy reach them. First
  create the Secret `ferrum-metrics-engine` (key `token`), the ConfigMap `ferrum-metrics-engine` (key
  `engine.json`, its paths under `/data`) and the claim `ferrum-metrics-datasets` (ReadOnlyMany) holding the
  datasets.
- **Pulling the image.** `ghcr.io/cbferrum/ferrum-metrics-engine` is public, so a cluster pulls it without
  credentials. Pin it by digest in your overlay; the `sha-<commit>` tags name the Metrics commit each was built
  from. A site without internet copies it into its own registry and maps it with `images:`.
- **In each workspace,** a Data Steward describes each dataset as an External Source in Metrics: which rows each
  reader sees, which columns only some roles see, and how fresh its data must be. Metrics asks the engine for
  one reader at a time, with only what that reader may see. The engine applies it in each dataset's own scan.
- **Down.** With the engine down, an external metric has no value and says so; nothing else waits on it.
  Before planning, it refuses a query whose terms nest more than 100 deep or that reads more than 32 models,
  since planning cannot be stopped midway; no query can overflow its threads' stacks.
  `GET /health` on port 5180 answers without a token. A value read from data older than its source allows
  carries the source under `stale`, and Intelligence says so in its answers.
- **Datasets** are files. Refresh them in place, by writing a new file and renaming it over the old one; the
  newest file's time is the dataset's age. They are not backed up with the stack: they are the lake's.

## Background jobs

Long work runs in the `job` table: bulk operations over many records, workspace exports, and whatever
apps register (`HookRegistry::add_job_handler`). Every replica with background loops takes jobs, up
to 4 at once; a workspace gets at most 3 running across the cluster, and each workspace with work
takes its turn.

| What happens | What the job does |
|---|---|
| Its replica dies | The lease (60 s) runs out and another replica takes it over. That counted as an attempt, so a job that kills its worker is given up on after `max_attempts` (5; 3 for exports) and stays `failed` for someone to look at, and the person who asked is told. |
| Its replica is stopped | It goes back in the queue, the attempt uncounted, before the replica exits. A deployment costs nothing. |
| It fails | Retried after 10 s, 20 s, 40 s and so on up to an hour. Something that cannot succeed (invalid, forbidden, not found) fails at once. Only the job's owner and administrators read `last_error`, and internal errors say so without detail: the detail is in the log, under the job's id. |
| It is cancelled | At once if it has not started. Otherwise within 5 s: the work is dropped at its next `await`, and its open transaction rolls back. What earlier transactions finished stays done. |
| Its owner left or lost a right | It runs as they are when it runs, so it fails, or the records they may no longer touch are listed in its result. A bulk job looks again at every 500 records. |

Jobs run at least once: a handler must be safe to repeat. Bulk jobs resume after the last 500 records
they recorded, so a lost worker repeats no more than those. Finished jobs are deleted after 30 days.

**Workspace exports.** The Workspace tab of Administration (or `POST /api/admin/export`) gives an
administrator a copy of everything: records with their child rows, what packages list in
`export_tables` (journals, stock moves), attached files, the audit trail, people, roles, permissions,
customizations and automations, in zip parts of about 128 MB of JSON lines and files, deleted after 7
days or by a personal-data erasure, whichever comes first (the export then says so). Passwords,
sign-in factors, secret fields, sealed settings, API keys and sessions stay out. `manifest.json` names
every workspace table it did not carry (`tables_left_out`) and every file the storage no longer had
(`files_missing`), which it goes on without: read it before relying on an export as a way out.

## Malware scanning

Set `FERRUM_CLAMAV_ADDR` to a ClamAV daemon (`clamav/clamav:stable`, listening on 3310; give it 2 GB of
memory, and keep it on the private network: it has no authentication). Every upload passes through it
before it is stored, whichever app it came through and whether it reached the server or went straight
to the bucket. What it finds is refused, is not stored, and goes on the audit trail as
`malware_refused`.

| What happens | What it means |
|---|---|
| Nothing set | Files are stored unscanned, and `ferrum task doctor` says so. |
| The scanner is down | Uploads fail with `503` until it is back: an outage must not be a way in. A direct upload, or one in parts, can be completed again; its bytes wait. |
| A file past its limit | Stored, marked `unscanned`, and logged. `FERRUM_CLAMAV_MAX_MB` (default 25) must match ClamAV's `StreamMaxLength`: raise both to `FERRUM_DIRECT_UPLOAD_MAX_BYTES` if every file must be scanned. Set above the scanner's real limit, a file between the two fails as if the scanner were down. |

`file_object.scan_status` says which files were scanned. Files stored before a scanner was configured
stay `unscanned`, and signatures added later do not reach files already stored.

## Checking a deployment

**Before the first start**, `deploy/preflight.sh` looks at the host and `deploy/.env`: Docker, secrets
left as placeholders, TLS, DNS, memory, disk and ports. `--generate` fills in missing secrets and
changes nothing else. Blockers exit non-zero; warnings do not.

**Once it runs**, `ferrum task doctor` reports on the deployment itself, blockers apart from warnings,
each with what to do. It connects to everything: PostgreSQL's version and connection limits, row-level
security on every workspace table, the clock, object storage (a write, read and delete), the malware
scanner, the PDF renderer, the online editor (set up, answering, offering docx, xlsx and pptx, signing its calls),
the queues, and every workspace's customizations. It does not send mail. An administrator sees the editor's and the
renderer's state without a shell at `GET /api/admin/services`.

```bash
docker compose -f deploy/docker-compose.yml exec ferrum ferrum task doctor
```

Add `format:json` for a machine. Each finding's level is `ok`, `off` (a service switched off on purpose),
`warning` or `blocker`. It exits non-zero on a blocker only, so a deployment can run it as a last
step. The report holds no secret or connection string, so it can be pasted into a ticket.

## Monitoring

`/metrics` (Prometheus text) exposes `ferrum_up`, uptime, database pool size and idle
connections, the outbox backlog (pending, dead, age of the oldest pending event), webhook
deliveries (pending, dead), the job queue (queued, running, failed in the last day, longest wait),
connections to outside systems (on, failing, waiting on a provider, with no success for a day) and
differences from a provider waiting for a person, the tenant count, and
`ferrum_tenant_refused_total`, requests refused because their workspace already had its share running
(`FERRUM_TENANT_REQUESTS`; the log line names the workspace), and
`ferrum_operation_failures_total{package, entity, operation, kind}`: submits, cancels, actions and
workflow steps on records that did not go through since the process started, `refused` by their own
rules (an invoice with no account to post to, a stale version) or a `fault` (logged with its cause).
Who may not do something, and records that are not there, are not counted. Take request rates and
latencies from the gateway controller's metrics. Suggested alerts:

| Alert | Condition |
|---|---|
| API down | fewer ready pods than `minReplicas` for 5 min |
| Error rate | gateway 5xx above 1 % for 10 min |
| Pool saturation | `ferrum_db_pool_idle` at 0 for 5 min |
| Outbox lag | oldest pending event older than 5 min |
| Dead letters | any dead outbox events or webhook deliveries |
| Failing jobs | `ferrum_jobs_failed_day` above 0 |
| Failing postings | `ferrum_operation_failures_total{kind="fault"}` rising; `kind="refused"` rising for one entity and operation is usually a setting people lack (a default account) |
| A workspace at its share | `ferrum_tenant_refused_total` rising: find the workspace in the log, and either it is misbehaving or it needs a larger share |
| Outside systems | `ferrum_connections_stale` above 0, or `ferrum_connections_failing` above 0 for 30 min |
| Database | CNPG: replication lag, failover events, storage above 80 %, connections above 85 % |

Traces go to an OpenTelemetry collector when the deployment names one: set
`OTEL_EXPORTER_OTLP_ENDPOINT` (for example `http://otel-collector:4318`; the server adds `/v1/traces`)
or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, and the SDK's other settings apply (`OTEL_EXPORTER_OTLP_HEADERS`
for a token, `OTEL_TRACES_SAMPLER`, `OTEL_BSP_SCHEDULE_DELAY`; `OTEL_SDK_DISABLED=true` turns them off).
They travel over OTLP/HTTP as protobuf, as `OTEL_SERVICE_NAME` (`ferrum` unless set). Each request is a
span with its method and path (never its query), and a caller's W3C `traceparent` makes it part of the
caller's trace. Without an endpoint nothing is sent and the log is as before.

Logs are JSON. Clients may send an `x-request-id` header to correlate their calls with the logs;
5xx bodies carry an `error_id` that appears in the log line with the full cause.

## Scaling

* API: the HPA scales 3–10 replicas on CPU (the connection budget above); replicas are interchangeable.
* Database: scale vertically first; add a PgBouncer pooler (`database.pooler.enabled` in the Helm chart,
  "Deployment shapes") when connections become the limit. Change the number of instances or the size of their
  volumes with `deploy/scale-database.sh --instances N` or `--storage 200Gi` (`--wait 600` to wait for
  replication to recover first). It changes nothing, says why, and exits 3 in these cases:
  - an instance is not ready;
  - a standby is not streaming, or is more than `MAX_LAG_BYTES` (16 MiB) behind;
  - fewer instances than `minSyncReplicas + 1` would be left, so writes would have no synchronous standby;
  - a volume would shrink, or the storage class cannot grow one.

  More instances are more copies to read from (`ferrum-db-ro`) and to fail over to. Every write still
  goes to the one primary (`ferrum-db-rw`): they are not more room for writes. `deploy/kind/smoke.sh`
  fences a standby, sees the change refused, then goes to two instances and back to three under writes. The journal is partitioned by year (partitions for the
  next year are created automatically); stock and audit tables are append-only.
* Very large tenants or many tenants: shard on `tenant_id` (Citus), which the schema already
  allows (every key starts with `tenant_id`).

## Deployment shapes

The Helm chart can run the server as several **pools**. A pool is one Deployment of the same server image;
pools differ only in settings (how many replicas, how many database connections, whether they run
background work, which zone they are in, which requests reach them). Every pool holds every app, and no pool
keeps anything of its own: the state is the database, the files and the keys. The decisions are in
[docs/plan/topology.md](plan/topology.md).

**Pools.** `pools:` in the chart's values. Empty (the default): one Deployment, rendered exactly as before.
The first pool keeps today's names and selector (`ferrum`), so an existing release upgrades in place; every
other pool is `ferrum-<name>`, with its own Deployment, autoscaler (or fixed `replicas`), disruption budget,
network policy and Service. A pool takes every setting it leaves out from the top-level values. Per pool:
`background` (`both`; `off`, which sets `FERRUM_RUN_BACKGROUND=false`, API only; `only`, which has no Service
and no routes and runs only background work), `zone`, `replicas`, `autoscaling`, `resources`,
`dbMaxConnections`, `pdb`, `env` (a name already set is replaced), `nodeSelector`, `tolerations`,
`priorityClassName` and `routes`. Each pool's pods carry the labels `ferrum.dev/pool` and `ferrum.dev/zone`
(never in a selector); the `ServiceMonitor` covers every pool with a Service and passes both labels on to
the metrics, and a `PodMonitor` (`ferrum-background`) scrapes the `only` pools.

**Presets.** Values files in `deploy/helm/ferrum/presets/`, given before your own values:

| Preset | Pools |
|---|---|
| `single.yaml` | One pool, as before |
| `split.yaml` | `web` (requests, no background work) and `worker` (background work only) |
| `pools.yaml` | `general` (the rest), `public` (the portal host), `finance` (the API calls of the apps it names), `worker` |
| `zones.yaml` | As `pools.yaml`, in two zones: the public gateway reaches only `public`, in a zone that admits no staff; staff work through a private gateway inside your network, where `general`, `finance` and `worker` are |

```bash
helm upgrade ferrum deploy/helm/ferrum -n ferrum -f deploy/helm/ferrum/presets/split.yaml -f my-values.yaml
```

**Moving between shapes.** `helm upgrade` with another preset, in either direction. The first pool's
Deployment, selector and Service stay, so its pods are replaced one at a time (none down before its
replacement is ready) and requests keep being answered; Helm creates the pools that are new and deletes the
ones that are gone. Background work is claimed through the database, so for a moment old and new pods may
both run it, which is safe, and nothing is lost when it pauses: going from `split` back to `single`, the
worker pool is deleted at once and background work waits until the first new `web` pod is ready. Moving to
`zones` changes where staff work: after it, staff use the private host, and the public host admits no staff.

**Connection budget.** Every replica may open `dbMaxConnections + 4` connections, and the migrate Job (when
it runs) as many again. With pools, the chart adds up every pool at its most replicas (`maxReplicas`, or
`replicas` without an autoscaler), the migrate Job and 20 for other users of the role (as
`scripts/check_manifests.py` does), and refuses to render when the sum is above `database.connectionLimit`.
Each preset says its sum. Without pools the chart refuses nothing new: check it with
`helm template ... | python3 scripts/check_manifests.py -`, which adds up every server Deployment and Job it
is given (and a pooler, as below). With the pooler on, the chart checks the budget with or without pools. (With today's defaults and `migrate.enabled`, that is 394 of 370: lower `autoscaling.maxReplicas`
to 9 when you turn the Job on.)

**Connection pooler.** `database.pooler.enabled` puts PgBouncer (CloudNativePG's `Pooler`, `ferrum-db-pooler`,
`database.pooler.instances` of them, at most one down at a time) in front of the primary, in transaction mode
with protocol-level prepared statements (`max_prepared_statements`, PgBouncer 1.21 or later; the operator
supplies the image). The server's pods then get `DATABASE_URL` pointing at the pooler and
`FERRUM_DATABASE_DIRECT_URL` at the primary (`ferrum-db-rw`), which the server uses for LISTEN, session locks
and migrations; the migrate Job always connects directly. Turn it on only with a server that reads
`FERRUM_DATABASE_DIRECT_URL`: an older one would send LISTEN and session locks through the pooler, where they
do not hold. Only the server's pods may reach the pooler, and Prometheus its metrics (`ferrum-db-pooler`
PodMonitor). With the pooler, PostgreSQL sees `instances x poolSize` connections from it (plus
`reserve_pool_size` if you set it in `database.pooler.parameters`) and `directConnectionsPerReplica`
(default 8: listeners and locks, to be confirmed by the server change) from each replica, and the budget
below counts that instead of `dbMaxConnections + 4`. `dbMaxConnections` is then what each replica opens to the
pooler; keep the sum over every replica under `maxClientConnections` times `instances`.

**What the chart refuses, and why:**

| Message | Meaning |
|---|---|
| `two pools are named …` | Each pool needs a name of its own: its objects are named after it |
| `… is the name of another part of the chart` | `pdf`, `db`, `office`, `editors`, `metrics-engine`, `migrate`, `background` and `private` are taken |
| `the first pool … cannot be background: only` | The first pool keeps today's Service, which the gateway and the online editor call |
| `zone … is not in zones` | A pool's `zone` must be one of `zones` |
| `its zone … admits no staff, so it must not run background work` | A pool in a zone with `staff: none` needs `background: off`; the server refuses to start such a replica anyway |
| `names the gateway …, which is not enabled` | A route names `public` or `private`, and that gateway must be on |
| `defaultPool … is not a pool` / `… has background: only` | A gateway's default pool must exist and take requests |
| `a pool with background: only … has no routes` | It has no Service to route to |
| `… takes all of the gateway's own hosts` | A route with no hosts, paths or apps: make that pool the gateway's `defaultPool` |
| `pools … both take all of host …` | Two pools cannot both answer the whole of one host |
| `the pools may open … database connections` | The connection budget above |
| `database.pooler … needs database.enabled` | The pooler is CloudNativePG's; in front of your own database, run PgBouncer yourself |
| `database.bootstrap.mode …: set …` | A way of starting the database lacks what it reads ("Moving from one shape to another") |
| `… where this cluster would also write its own` | A recovery from this chart's own backups needs a new `database.backup.serverName` |

**Two gateways.** The public Gateway is as before (`gateway`). `privateGateway.enabled` adds a second one,
`ferrum-private`, with its own class, hosts, certificate and Envoy proxies; `privateGateway.envoyProxy.envoyService`
holds the proxies' Service settings, where your cloud's annotation for an internal load balancer goes (examples
in `values.yaml`). A pool's `routes` say which gateway reaches it and by what: whole `hostnames` (added to that
gateway's listeners, with its certificate), path prefixes (`paths`), or `apps`, matched on the `X-Ferrum-UI`
header (`{app}@{version}`, which every API call carries) with an anchored regular expression. What no route
takes goes to the gateway's `defaultPool` (the first pool unless set). WebSockets, files and MCP keep their own
routes and time limits on every gateway; they carry no app, so they follow the host or the gateway's default.

**Network policies.** With pools, each pool admits only the Envoy proxies of the gateways whose routes lead to
it (Envoy Gateway's `gateway.envoyproxy.io/owning-gateway-name` and `…-namespace` labels, in
`networkPolicy.gatewayNamespace`), Prometheus (`networkPolicy.monitoringNamespaces`) and, for the first pool,
the online editor. A `background: only` pool admits no gateway. This relies on Envoy Gateway running its
proxies in its own namespace, its default. `networkPolicy.egressAnyHost: false` replaces "any host on 443 and
587" with the networks in `networkPolicy.egressCIDRs`; list object storage, mail and every service your
connections call.

**Zones.** `zones` becomes `FERRUM_ZONES`, `zoneDefault` `FERRUM_ZONE_DEFAULT`, `zoneMode` `FERRUM_ZONE_MODE`,
and a pool's `zone` its `FERRUM_ZONE` (the zone every request to that pool is in). What the server does with
them is in [docs/plan/topology.md](plan/topology.md), D4.

**Secrets from a secret store.** `secrets.externalSecret.enabled` makes the server's Secret
(`secrets.existingSecret`) with the External Secrets Operator, from `dataFrom` or `data` in the store
`secretStoreRef`. The operator is yours to install. Pods read the Secret as they start: restart them after a
value changes.

**Not proven.** The chart has only been rendered and checked (`deploy/helm/test.sh`,
`deploy/helm/test_pools.sh`); none of these shapes has been installed on a cluster. Whether Envoy Gateway
orders the app routes over the default ones as the Gateway API says, whether the network policies admit
exactly the private gateway's proxies, the internal load balancer annotations, a move between presets under
traffic, and the External Secrets Operator have not been run.

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

### Reference workload and targets

The reference workload is `tools/loadtest.py`'s mix, signed in as one person of one workspace, each
client sending its next request when the last is answered:

| Call | Share |
|---|---|
| List 20 customers | 30% |
| List 20 sales invoices, newest first, with titles | 20% |
| Read an item | 20% |
| An entity's metadata | 10% |
| Live totals of an invoice (`_calc`) | 10% |
| Create a draft Sales Invoice (1 to 3 lines) | 10% |

Every image build runs it for 20 seconds at 10 and then 50 clients, against the image deployed
with the Compose stack on CI's runner (`deploy/smoke.sh`, the `image` job): the server, PostgreSQL,
the editors' server and LiveKit share one GitHub-hosted machine with the load generator. The build
fails when a level misses a target:

| Clients | Errors | p95 at most | Requests a second, at least |
|---|---|---|---|
| 10 | 1% | 300 ms | 120 |
| 50 | 1% | 2,000 ms | 100 |

The targets leave room for a shared runner's variance. On 2026-11-06 (run 42135552923, `93d6faf`),
10 clients got 349 requests a second at p95 72 ms (p99 113 ms), and 50 clients 272 a second at
p95 518 ms (p99 764 ms), with no errors. The targets catch a release that got slower than that by
several times. They are not a sizing guide: a production replica has its own cores and database.
To hold a deployment to its own targets, run the same mix against a staging copy with
`--p95-ms` and `--min-rps` set for that hardware, and `--summary` to keep each level's figures.

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

### One workspace flooding the others

`scripts/share_check.py` has one workspace send all it can from 64 threads while another's reads are timed.
On 2026-11-03, a debug build on the same laptop with 10 database connections, 500 reads of 100 contacts:

| | The other workspace alone, p50 / p95 | During the flood, p50 / p95 | The flood |
|---|---|---|---|
| Share on (5 of 10) | 8.8 / 13.2 ms | 12.6 / 24.8 ms | 3,366 requests in 8.6 s, each waiting a median 128 ms for its turn, none refused |
| Share off (`FERRUM_TENANT_REQUESTS=0`) | 8.3 / 14.1 ms | 116 / 145 ms | 32,667 requests in 61.7 s |

The share is counted on each replica, so with three replicas a workspace may hold half of each. It counts
requests through the API, from when the workspace is known until the response is made: a WebSocket or a
stream holds no turn once it has started. A single expensive query is bounded by the statement timeout, not
by the share, and the database's CPU is not divided between workspaces.

## Incident playbooks

**Database failover.** Automatic. In-flight transactions fail with a retryable error (clients see
`503 retry`; a create, an update or an action sent with an `Idempotency-Key` can be sent again with the
same key and is done once). Check `kubectl cnpg status ferrum-db`. While the database cannot be read,
the assistant runs on each workspace's policy as last read for up to five minutes, then says it is
unavailable rather than run unrestricted.

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
erasure. Administration → Personal data finds what the installed packages declare as theirs, under
every address they have had: download the JSON for an access request; for an erasure, give the
request's reference as the reason and erase. Their details leave the records listed and those
records' audit history (the events keep their place in the hash chain and record who redacted what,
when and why), files attached to those records are deleted, and their login account is anonymized
and closed, with its sessions, keys, notifications, preferences and devices. An administrator's
account is not erased until it is no longer one. What durable processes keep of the records erased (the
event that started a run, the run's own record, the material shown) loses those fields too, and keeps them out
when the run is reported again. The same is available as `GET
/api/admin/personal-data` and `POST /api/admin/personal-data/erase`; packages and apps declare their
entities with `personal_data` in their manifests. What an app keeps of people in tables of its own,
outside its entities (a meeting's transcript, say), it reports and erases itself
(`ferrum_kernel::privacy::PersonalHeld`): it is in the copy under `apps`, goes with the erasure in the
same transaction, and goes again when a restore replays the erasure. A name asked for is matched whole
or as one of the names a field lists (`Ann Smith, Pat Lee`), and such records are only possibly theirs.

Read the answer, because it says what was not done:

| Key | Means |
|---|---|
| `retained` | Records a package keeps, each with `because`. They were exported and left as they are. Deciding that they may stay is yours or your counsel's, not Ferrum's. |
| `held` | Records a legal hold covers, left as they are. |
| `not_searched` | Installed packages that declare nothing, so were not looked at. |

**Legal holds** stop erasure and deletion while a matter is open: `POST /api/admin/legal-holds` with
`{"email": "...", "reason": "..."}` for a person, or `{"entity": "...", "record_id": "...", "reason":
"..."}` for a record; `GET /api/admin/legal-holds`; `POST /api/admin/legal-holds/{id}/release`. They
are audited, and released holds stay listed.

A hold on a record: the database refuses to delete it, whatever asks (an app's own SQL included), and
whenever anything changes it the version it had is kept, for administrators to read with
`Kernel::held_versions` until the hold is released, when those versions go and the release says how
many. A hold on a person keeps their records, under any address they have had, from erasure, deletion,
retention rules and an app's removal with its data; to keep the earlier versions of one of their records
as well, place a hold on that record. Removing an app with its data (`DELETE /api/admin/apps/{id}?delete_data=true`)
and deleting a workspace (`ferrum task delete_tenant`) are refused while a hold covers what they
would destroy: release the hold first, which is recorded, and say why.

Restriction of processing, as an operation of its own, is not built.

**Restricting processing** keeps someone's data and stops using it: `POST /api/admin/restrictions`
`{email, reason}` covers every address they have had. No automated message goes to them, asked again
as each one goes out, so mail queued before the restriction stops too; apps with their own delivery ask
`Kernel::may_contact_for` the same way. It deletes nothing, and hides nothing from people who can read
the records. `POST /api/admin/restrictions/lift` undoes it.

**Consent to marketing** is one record per address (`contact_consent`), whichever app heard it: an
unsubscribe from a campaign, a confirmed subscription from a form (`Kernel::record_consent`, audited).
Every path that sends marketing asks `Kernel::may_contact_for(.., Purpose::Marketing)` as the message
goes out: kernel mail queued for marketing, an automation's e-mail whose `purpose` is `marketing`, a
process's `ferrum.mail.marketing` capability, and a connector stream that names its `consent` field
(the address of each person it sends to a marketing service). A withdrawal stops all of them, under any
address the person has had, including what was already queued. Transactional notices (receipts,
resets, replies) do not ask for consent; a restriction stops them as well. When a record declared as
someone's changes its address (a customer's, a contact's), their consent and any restriction follow them
to the new address, unless that address has a word of its own; a record that now names someone else errs
on the side of not contacting them until their own subscription says otherwise.

**Retention rules** say how long an entity's records are kept: `POST /api/admin/retention` `{entity,
after_days, date_field, action: delete | anonymize, condition, reason}`. They run once a day as a job,
5,000 records a rule at most (`KernelSettings::retention_daily_records`), each day going on from where
the last stopped; a scan that reaches the end starts again, and a changed rule starts afresh. A legal
hold, on the record or on its person, stops a rule for that record; what a package keeps for a stated
reason cannot have a rule that deletes it; and a record that cannot be deleted is listed in
`last_result` (`held`, `failed`, `failures`, `scan`, `more`), passed over, and tried again when the scan
comes round.
`POST /api/admin/retention/{id}/preview` says what a rule would reach, without touching it. Immutable
financial history is out of their reach by design.

## Online editing

Drive opens and co-edits Word, Excel and PowerPoint files (`.docx`, `.xlsx`, `.pptx`, and `.odt`,
`.ods`, `.odp`, `.rtf`, `.csv`) in an editor server beside Ferrum: Collabora Online, or ONLYOFFICE Docs in
WOPI mode. **It is on in every deployment this repository ships**, as are the PDF renderer
(Gotenberg) and, for Drive's "New document, spreadsheet, presentation", nothing else to install. The
editor reads and saves through Drive's WOPI endpoints with each person's own rights, and every save
becomes a new version. Details are in the Drive app's README.

**What runs.** The Compose stack has an `office` service (`collabora/code`, an exact tag, `COLLABORA_TAG`)
and the Helm chart and `deploy/k8s/office` a Deployment `ferrum-office` of the same image. The server
reads these settings (the stack and the chart set them):

| Setting | On | Value |
|---|---|---|
| `FERRUM_OFFICE_URL` | server | the editor as the server reaches it: `http://office:9980` (Compose), `http://ferrum-office.ferrum-office.svc.cluster.local:9980` (Kubernetes) |
| `FERRUM_OFFICE_WOPI_URL` | server | the server as the editor reaches it: `http://ferrum:5150`, `http://ferrum.ferrum.svc.cluster.local:80` |
| `FERRUM_OFFICE_ORIGIN` | server | the editor as browsers reach it, an origin of its own (`https://office.example.com`); Drive's pages may frame it and post to it, and their Content-Security-Policy names exactly it |
| `FERRUM_OFFICE_IPS` | server | optional: the only addresses WOPI calls are accepted from |

**The switches.** One setting turns each service off. *Compose*: in `deploy/.env`, `FERRUM_OFFICE_URL=`
(set, empty) turns the editor off and its container does not start; `FERRUM_PDF_URL=` does the same for
the PDF renderer. For an editor of your own, set `FERRUM_OFFICE_URL`, `FERRUM_OFFICE_WOPI_URL` and
`OFFICE_REPLICAS=0` (the bundled one does not start). *Helm*: `office.enabled: false` and `pdf.enabled: false`;
nothing of the editor is rendered, and the server is not told about it. *Kustomize*: leave out
`kubectl apply -k deploy/k8s/office` (the server then says, in Administration, that online editing is
off) or remove `FERRUM_OFFICE_*` from `deploy/k8s/deployment.yaml`.

**The editor's address.** The editor must have an origin of its own: another host name, or on one machine
another port. It serves a full web application that must never share Ferrum's origin, where it could
reach people's sessions. So:

* *Compose*: `OFFICE_PORT` (default 9980) is the one published port; set `OFFICE_PUBLIC_URL` and
  `OFFICE_PUBLIC_HOST` to the address browsers use, and `OFFICE_TLS_TERMINATED=true` when your TLS proxy serves it
  over HTTPS. Behind your own proxy, publish it on `127.0.0.1` (`OFFICE_BIND`) and give it a host name of its own
  (`office.example.com`), proxying WebSockets (`/cool/…` carries them, with no overall time limit). Do not proxy
  `/cool/adminws`, `/cool/getMetrics`, `/cool/convert-to` or `/browser/dist/admin`, and `/hosting/discovery` is
  for the server, not for browsers.
* *Kubernetes*: a host of its own next to Ferrum's (`office.example.com`, `office.host`), served by the same
  Gateway with a certificate cert-manager issues. Its HTTPRoute sends only `/browser` and `/cool` to the editor,
  without a time limit for the WebSockets, and sends the paths above back to Ferrum's home page.

Drive's pages carry a Content-Security-Policy that names this origin in `frame-src` and `form-action` and
nowhere else (`FERRUM_OFFICE_ORIGIN`); Ferrum's own policy is not widened. The editor's pages carry
`frame-ancestors` of Ferrum's address alone (`net.frame_ancestors`, from `PUBLIC_URL`).

**Security.**
* *One WOPI host.* The editor is told Ferrum's address as the only host it serves (`aliasgroup1`, from
  `FERRUM_OFFICE_WOPI_URL`; no wildcard): a document from any other host is refused. It does not trust the first
  host that calls, which is its default when none is named.
* *No console.* The administration console is off (`admin_console.enable=false`) and its paths are not routed.
* *Proof key.* Give the editor a key and every WOPI call must carry its signature over the token, the URL and the
  time, so a leaked token alone opens nothing. `deploy/preflight.sh --generate` makes `deploy/office-proof-key` and
  sets `OFFICE_PROOF_KEY`; or `openssl genrsa -traditional -out proof_key 2048`. On Kubernetes, make a Secret
  `ferrum-office-proof` with that file as key `proof_key` in the editor's namespace (`kubectl -n ferrum-office create secret generic ferrum-office-proof --from-file=proof_key`)
  and restart the pod. Without a key the editor logs that it found none, Administration says its calls are not
  signed, and the defences are the tokens (per person and file, ten hours) and the network (`FERRUM_OFFICE_IPS`, network policy).
* *Container.* It runs as user 1001 with every capability dropped but the four its document jails take
  (`CHOWN`, `FOWNER`, `MKNOD`, `SYS_CHROOT`). Its jail helper carries file capabilities, so `no-new-privileges` /
  `allowPrivilegeEscalation: false` cannot be set (the editor does not start with them). That is why Kubernetes runs it
  in a namespace of its own at Pod Security level `baseline`: the `ferrum` namespace enforces `restricted`.
* *Network.* In Kubernetes only the Gateway's proxies and the server reach the editor, and it reaches only the server
  (`ferrum-office` network policy) and DNS: nothing on the internet. In Compose its one published port is the editor.
  The server's policy admits the editor's pods on 5150.
* *Third parties in the browser.* The editor's own pages name `www.zotero.org`, `rating.collaboraonline.com`
  and `www.collaboraoffice.com` in their policy (citations, a rating frame, branding images). Ferrum sends nothing there.
* *Conversion service.* The editor can convert uploaded files (`/cool/convert-to`); it admits callers by address
  and a gateway's address is one it admits. Kubernetes keeps that path off the route; with your own proxy, do too.
* Logs go to the container's standard output (`warning` level; raise it with `--o:logging.level=` in
  `extra_params` for a problem). Request logs of the server record paths without query strings, so WOPI tokens never reach them.

**Sizing.** Measured on the local environment (Collabora CODE 26.04.5.2.1, arm64, small files: a budget sheet with a
chart, a two-slide deck, a one-page policy with a table and a picture): the editor takes about 590 MiB at rest and
about 50 to 60 MiB more for each open document (six open at once: 921 MiB), and gives it back within about 20 seconds
of the last person leaving. Two people in one spreadsheet cost no more than one (643 MiB). Large spreadsheets, decks
with many pictures and long documents cost more; that was not measured, and neither was CPU. The Compose limit is
`OFFICE_MEMORY` (2 GB), the Kubernetes request 1 GiB and limit 3 GiB (`office.resources`). Plan from the per-document
figure with room for larger files, and watch the editor's memory in your own monitoring before you trust a number for
many simultaneous editors. The editor's own configuration frees idle documents when it reaches 80% of its memory limit
(`memproportion`; taken from its default configuration file, not provoked here). One replica only: it keeps each open
document in the pod that opened it, and a Gateway cannot route by document, so size the pod up rather than out (above
one replica the chart adds a PodDisruptionBudget, and each document is then in whichever pod it reached: you need routing
on `WOPISrc`, which the Gateway API cannot express). Saves are limited by Drive's upload limit
(`FERRUM_DRIVE_MAX_FILE_MB`, 32 MB unless raised); the editor itself does not limit a file's size
(`storage.wopi.max_file_size` is 0).

**Licence and support.** The image is Collabora Online Development Edition (CODE). It is free software (MPL-2.0), and
what it says about itself on opening is that it is "free for you to use, to explore and to use with others in the
browser" and "aimed at home users and small teams". It has **no vendor support**: Collabora's supported builds are a
subscription, or you build the project from source; both run in the same place (change `office.image.repository` and
the tag in Helm, `image:` and `COLLABORA_TAG` in Compose). CODE shows a welcome notice and a tour to each person once
per browser, and cannot be configured to stop: its configuration file says the welcome screen and feedback pop-ups can only
be switched off by "home mode", which limits the instance to 20 connections and 10 open documents. That is part of what
you accept by running CODE for an organization. The tag is exact on purpose (an upgrade is your decision). Whether CODE
is enough for your use of Ferrum is yours to decide; Ferrum only speaks WOPI to it.

**ONLYOFFICE instead.** Drive speaks WOPI, and ONLYOFFICE Docs has a WOPI mode with its own discovery document at
`/hosting/discovery`. To use it: set the bundled editor off (`FERRUM_OFFICE_URL=` and run your own, as above),
run ONLYOFFICE Document Server 8 or later with its WOPI enabled and its allowed WOPI host set to
`FERRUM_OFFICE_WOPI_URL`, point `FERRUM_OFFICE_URL` and `FERRUM_OFFICE_ORIGIN` at it, and
check with `deploy/office_check.py` and Administration. Not tested here: Ferrum has no ONLYOFFICE image of its own
and this repository's checks run against Collabora. ONLYOFFICE publishes no list of participants, so Drive's
presence shows people from its own heartbeat instead.

**Checking it.** *Before the first start*, `deploy/preflight.sh` checks the settings, makes the proof key, and (once the
editor runs) that its discovery document offers an editor for docx, xlsx and pptx. *Once it runs*, Administration
shows whether the editor and the PDF renderer are set up and answering (`GET /api/admin/services`, as an administrator:
`{services: [{check, level, detail, fix}]}`, with `office`, `office.proof` and `pdf`; `level` is `ok`, `off` (switched off
on purpose, its address left empty: information, with no `fix`), `warning` or `blocker`), and `ferrum task doctor` prints the
same. `deploy/office_check.py --url … --tenant … --token … --office …` opens a docx, an xlsx and a pptx made in Drive,
does what the editor does over WOPI (CheckFileInfo, LOCK, GetFile, PutFile, UNLOCK), checks the save became a new
version, and has the editor load a file itself; `deploy/smoke.sh` runs it on the Compose stack, and `deploy/kind/smoke.sh`
checks the Kubernetes routes, the network policy and the server's report. With the editor down, Drive's "Open" answers
that the editor server cannot be reached (it names the failure, not the address); everything else in Drive works, and
files open in the browser editors if they are installed.

**Things to know**
* **People in the editor:** the editor tells Drive's page who is in a file. Its Share, version
  history and close buttons open Drive's own, through messages to Ferrum's origin. The server
  names that origin from its `PUBLIC_URL`, which must be the address people open: without it, the
  editor keeps its own buttons.
* **Logs:** request logs record paths without query strings, so WOPI tokens and sign-in codes never
  reach them.
* **Rate limits:** the editor calls Ferrum from one address, so the public endpoints' per-address
  limit applies to all of its sessions together. Raise it for busy editors, or exempt the editor's
  address at the ingress.
* **First use:** Collabora shows a welcome notice and a tour once per browser (see Licence). `allow_update_popup` is
  off here, since updates are the operator's.
* **Theme and language:** the editor does not take Ferrum's appearance or language setting. In the checks it followed the
  browser's colour scheme from the second session on and stayed light in the first, and showed the browser's language (English
  (UK)). Its address takes `ui_theme=light|dark` and `lang=<code>` (both checked: `lang=ar` mirrors the whole editor);
  Drive does not send them yet.

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
  key and secret that you give both. It calls LiveKit only for recordings, when they are on.
* Who may join: a channel's call, whoever may read the channel; an event's meeting, its organizer
  and guests (anyone for a public event); a room, anyone in the workspace with its link. The app's
  README ("Meetings") has the details.

| Setting | On | Value |
|---|---|---|
| `FERRUM_MEET_URL` | server | LiveKit as browsers reach it, `wss://meet.example.com` |
| `FERRUM_MEET_KEY`, `FERRUM_MEET_SECRET` | server | an API key (a name) and its secret, at least 32 characters (`openssl rand -hex 32`); LiveKit gets the same pair |
| `FERRUM_MEET_RECORDINGS` | server | `true` once [LiveKit Egress](https://docs.livekit.io/home/egress/overview/) (Apache-2.0) runs beside LiveKit with its storage: anyone in a meeting may then record it |
| `FERRUM_MEET_API_URL` | server | LiveKit's server API as Ferrum reaches it, when that is not `FERRUM_MEET_URL` over http(s): an address inside your network |

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

The server's network policy needs no change unless recordings are on: then Ferrum calls LiveKit's
API (`FERRUM_MEET_API_URL`, or LiveKit's address) to start, stop and list them, and the policy must
let it.

**Captions, transcripts and recordings.**
* Captions are each person's own. *Caption my voice* has their browser's speech recognition caption
  what they say (Chrome, Edge and Safari have it). The button says that the browser may send the voice
  to its maker to recognise it.
* Captions go to the call on LiveKit's `lk.transcription` text stream. A LiveKit transcription agent
  you run (LiveKit Agents, with the speech-to-text you choose) publishes on the same stream and shows
  the same way, though Comms does not keep what it says.
* What people caption is kept as the meeting's transcript, for whoever may join the meeting.
* Recordings are LiveKit Egress's: run it with its storage (an S3 bucket, say), its own way. Ferrum
  asks it to record a meeting's room to `ferrum/{workspace}/{room}/{recording}.mp4`, one recording at a
  time. Everyone in the call sees that it is recording. Each meeting lists where its recordings went.
  Who may open them is your storage's to decide.

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

## Live video

The Live app (`ferrum-app-live`) sends one presenter's video to a large audience at low delay through a
[Media over QUIC](https://github.com/moq-dev/moq) relay (`moq-relay` 0.17.16, Apache-2.0 or MIT) that you run beside
Ferrum. It is optional: without a relay the server starts and Live's records work, and broadcasts cannot go live.
Sound and pictures never pass through Ferrum; Ferrum signs short-lived tokens (HS256) that the relay's auth server
checks, and counts viewers from their token renewals. The app's `docs/deploy.md` has the whole story.

| Part | Image | Listens on |
|---|---|---|
| Relay | `moqdev/moq-relay:0.17.16` | UDP 443 (QUIC, WebTransport) and TCP 443 (HTTPS, WebSocket fallback), the only ports the internet reaches; plain HTTP 8080 for `/health`, inside the network |
| Auth server | `moqdev/moq:0.14.16` (`moq auth serve`) | `127.0.0.1:4440` in the relay's own network namespace, published nowhere (the relay accepts `http://` for loopback only) |

**Compose.** `deploy/live.yml` adds both:

    docker compose -f deploy/docker-compose.yml -f deploy/live.yml --env-file deploy/.env up -d

* Before the first start, in `deploy/live/` (beside `relay.toml`): `cert.pem` and `key.pem`, a publicly trusted
  certificate for `FERRUM_LIVE_HOST` (browsers refuse a self-signed one for WebTransport), and `keys/<kid>.jwk`:
  `docker run --rm -v "$PWD/deploy/live/keys:/out" moqdev/moq:0.14.16 auth generate --id ferrum-1 --out-dir /out`.
  None of them is committed (`.gitignore`).
* In the app's Live Settings: *Relay Address* `https://<FERRUM_LIVE_HOST>`, *Relay Health Address*
  `http://live-relay:8080/health`, then *Set Signing Key* with the key file's text (sealed, never shown again); the
  app's setting *Relay address for browsers* to the same `https://` address, so its pages may connect there.
* Raise the Docker host's UDP buffers (`net.core.rmem_max` and `wmem_max` = 8388608).
* Rotating the key: add a new `<kid>.jwk` beside the old one, *Set Signing Key* with it, remove the old file once
  tokens signed with it have run out (60 minutes at most).

**Kubernetes.** The app's `deploy/k8s/live.yaml`: relay and auth server in one pod, a LoadBalancer with UDP and TCP
on 443, the secrets `live-relay-tls` and `live-relay-keys`. The platform's chart has no component for it, as for
meetings.

**Health.** Live's doctor check (the support report, `ferrum task doctor`) says when Live Settings names no relay,
holds no usable key, or the relay's `/health` does not answer. Nothing here was run against a real relay.

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
| `FERRUM_ASSISTANT_API_KEY` | the model service's API key; without it there is no assistant |
| `FERRUM_ASSISTANT_PROTOCOL` | `anthropic` (default): Anthropic's Messages API, or a proxy that speaks it; `openai`: an OpenAI-compatible chat API (`/v1/chat/completions`: OpenAI's, Azure's, or a model served on the operator's own machines), with text, tools and usage; reasoning, citations and fallbacks are Anthropic's and are not sent |
| `FERRUM_ASSISTANT_MODEL` | default `claude-opus-5-5` |
| `FERRUM_ASSISTANT_EFFORT` | `low`, `medium` (default), `high`, `xhigh` or `max`: how hard the model thinks, and so its speed and cost |
| `FERRUM_ASSISTANT_URL` | default `https://api.anthropic.com`; a proxy or gateway that speaks the same API, or the provider's regional endpoint: a deployment is one region, and calls the one its operator sets |
| `FERRUM_ASSISTANT_FALLBACKS` | `on`/`off`; on by default for Anthropic's API with a model that has them: a request the model's safety checks decline is answered by Anthropic's recommended other model in the same call |
| `FERRUM_ASSISTANT_MODELS` | other models a workspace may choose, comma-separated |
| `FERRUM_ASSISTANT_MONTHLY_TOKENS` | the most one workspace may use in a calendar month; it can set less |
| `FERRUM_ASSISTANT_CALLS_PER_HOUR` | calls to the model one person may make in an hour, default 120 (a question that reads several things is several calls) |

* **Checking it.** After setting the key, `ferrum task assistant_check` sends the model one short
  message (no tools, no workspace data) and says which model answered, or why the key, the model
  or the network does not work.
* **What leaves the server.** The person's messages, the page they are on, and what the tools
  read for an answer (records, reports) go to the model's API, and nothing else: the assistant
  sees only what the person may see. Set the key only where your data policy allows it; Anthropic's
  API terms and retention apply to what is sent.
* **Per workspace.** Administrators turn it off for their workspace from the panel's
  Conversations menu (or `PUT /api/admin/settings {"assistant": false}`); every replica follows at
  once (within 20 seconds where its notifications are not reaching it). A policy that cannot be read
  is never the defaults: the one last read stands in for up to five minutes of an outage, never once the
  settings are known to have changed, and otherwise the assistant is unavailable. Guests and portal
  people never have it.
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


**What may leave.** Reading something does not make it the person's to send to a third party. So
everything the assistant reads is narrowed, whoever is asking, an administrator included: entities
marked `internal` are refused, and fields in a permission group or holding a secret are left out, in
records, lists, filters, sorting, reports and audit trails. A workspace widens this deliberately with
the `assistant_egress` setting, `{groups: [...], entities: [...]}`. Secrets never go.

**What it may do.** A change waits for the person. What they allow is the call as it was shown, on the
record as it stood: if the record changed meanwhile, or 30 minutes passed, nothing happens and the
assistant has to look again. A model that does whatever a ticket tells it can read what the person's
narrowed view allows, and can propose. `server/tests/assistant_egress_it.rs` runs exactly that.

A call runs at once only when this server knows it reads: Ferrum's own reading tools, a capability
whose definition only reads, or an action declared read-only (run in a read-only transaction). What a
tool says of itself (its `readOnlyHint`) does not count. Every input is checked against its tool's
schema at every depth before anything runs: types (null only where allowed), required and unknown
fields, enumerations, bounds, lengths and formats.

**While it answers.** Before every call to the model, every retry and every tool, the person and the
workspace's policy are read again: a person signed out everywhere or turned away, the assistant turned
off, or a change to what the person may see or do (roles, scopes, what may go to the provider) stops
the turn there, and the calls it left are answered as not run. `server/tests/assistant_boundaries_it.rs`
takes a role away between two rounds of calls and turns the assistant off during a retry.

**Earlier in a conversation.** A new turn sends the model the person's earlier words, but never the
earlier calls, their results or the page context, and an earlier answer only while the person may
still read every record and kind of record it drew on, and only to the destination it went to under
the same egress policy (each answer records both). An answer that no longer qualifies is replaced by a
line saying it was left out. A kept conversation is an archive, not context reused as it was.

**Shown and kept.** A key the model writes (a Ferrum API key, a provider's key, a private key) is
shown and kept as `[withheld]`. Text streams to the page short of where such a key could still be
starting, so one cut across the pieces it streams in is caught as well.

**How much.** Workspaces set `assistant_model` from the models the operator allows, and
`assistant_monthly_tokens`, which cannot exceed the operator's. Before each call, what it may use (its
input and the most it may answer) is held back from what is left, under one lock per workspace, and the
answer is capped at what is left; with less than 1,024 tokens to answer with, the call is not made.
Turns at once cannot spend more than the allowance between them. What a call holds is let go once its
use is recorded or it fails; one left by a server that stopped counts for 15 minutes
(`assistant_reservation`).
`GET /api/assistant/usage` shows this month's tokens by person and model. The log records each call's
model, tokens and tool names, and never what was said.

**What it was told.** Each answer records the version of its instructions, tools and model
(`prompt` in its usage); `GET /api/assistant/prompts/{version}` gives an administrator that
version's text and how many answers it gave.

**How well it answers.** `ferrum task assistant_eval slug:<workspace> email:<person> scratch:yes`
asks the model the grounded-answer cases in `server/assistant-evals/grounded.yaml` (or `file:` a
dataset of your own) as that person, and passes a case when the answer says what the records say,
read them first, and says nothing a record planted for it to say. Each case adds its records to
the workspace, so give it one set aside for this. Every run costs tokens.
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

## Portugal: certified invoices

Portuguese companies' invoices are signed as the AT requires of certified software, with the
software producer's key. The server reads (keep them with the deploy secrets):

| Variable | Meaning |
|---|---|
| `FERRUM_PT_SIGNING_KEY` | The producer's RSA private key (PEM, PKCS#8 or PKCS#1, newlines included) whose public key was given to the AT at certification |
| `FERRUM_PT_CERTIFICATE` | The software's AT certificate number, printed on every invoice ("Processado por programa certificado n.º …/AT") |
| `FERRUM_PT_KEY_VERSION` | The key's version, recorded on each invoice it signs (default `1`) |

* **Without them** submitting a Portuguese company's invoice refuses with an error; other countries
  are unaffected. Set them before a workspace turns Portugal on.
* **Rotating the key:** give the AT the new public key, then set the new key and raise
  `FERRUM_PT_KEY_VERSION`; earlier invoices keep the version that signed them.
* **Series:** each document series is communicated to the AT first and entered as a PT Series with
  the AT's validation code; a series' numbers and its hash chain never restart.

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
* **Developing an interface.** On a development server run with `FERRUM_DEV_UI=true`: in the app's
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
  it uses plain http. On a developer's machine, whose base domain is `localhost`, a name under it
  (`shop.localhost`) is accepted and works at once, since it can only reach that machine, and is not
  checked again.
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


**Single sign-on (SAML 2.0).** For providers that speak SAML (AD FS, Entra ID, Okta, OneLogin,
PingFederate, Shibboleth, Keycloak…). Administration → Single sign-on shows the workspace's metadata
address, `{PUBLIC_URL}/api/auth/saml/{workspace id}`, which is also its entity ID, and its assertion
consumer service (`…/acs`, HTTP-POST): give the provider the metadata address, or those two. Have it
sign its responses or assertions (RSA-SHA256 or better) and send the person's email address, in an
attribute (`email`, `mail`, Entra's `…/claims/emailaddress`, the X.500 mail identifiers) or as the
NameID. A persistent NameID links the person to their identity there; a transient one makes the
address stand in for it. Then add the provider with its metadata, pasted or its https address
(read again at each sign-in, so a new signing certificate is picked up), and the email domains it
vouches for, which SAML needs listed. Requests are signed with the workspace's own key (made when
first wanted, sealed by `FERRUM_SECRET_KEY`, its certificate in the metadata; providers that
import the metadata, Keycloak among them, then check every request). It needs `PUBLIC_URL`. Not
supported: single logout, encrypted assertions (turn the provider's encryption off; HTTPS protects
them), and sign-ins the provider starts unasked.

The release image is built with SAML (`--features saml`). Building it yourself needs libxml2,
libxmlsec1 with its OpenSSL backend, pkg-config and libclang (Debian and Ubuntu:
`libxml2-dev libxmlsec1-dev libclang-dev pkg-config`; running it: `libxml2 libxmlsec1
libxmlsec1-openssl`). On macOS with Homebrew (`brew install libxmlsec1 libxslt`), one libxml2 must
be linked, Homebrew's:
`PKG_CONFIG_PATH=$(brew --prefix libxslt)/lib/pkgconfig:$(brew --prefix libxml2)/lib/pkgconfig:$(brew --prefix libxmlsec1)/lib/pkgconfig:$(brew --prefix openssl)/lib/pkgconfig`,
`OPENSSL_DIR=$(brew --prefix openssl)`, `LIBXML2=$(brew --prefix libxml2)/lib/libxml2.dylib` and
`LIBCLANG_PATH=/Library/Developer/CommandLineTools/usr/lib`. `ferrum task doctor` reports
`identity.saml`: whether xmlsec1 signed and verified a response on this machine.

**Signing out at the provider.** A provider that supports OpenID Connect Back-Channel Logout can end
a person's sessions here when they sign out there or it disables them. Give it
`{PUBLIC_URL}/api/auth/sso/{provider id}/backchannel-logout`. The logout token's signature, issuer,
audience, event and age are checked, and each token works once. It ends all the person's sessions at
Ferrum and their refresh tokens, on every replica, not only the session it names. Their API keys stay:
those are not sign-ins, and removing the person ends them.
## Industrial sites

A site runs `ferrum-site` next to its equipment: [running a site](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/gateway.md). Who may command
what is in [operational authority](operations-authority.md), and what has been tried on what in
[compatibility](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/compatibility.md).

After restoring the platform from a backup, sites may be ahead of it: their reports replay
harmlessly, and commands issued after the backup was taken are gone from the ledger though a site may
have run them. Before enabling dispatch, compare each site's console with **Needs a person**.

## Taking one workspace back

Restoring a shared database takes everyone back, which is rarely what one workspace's accident calls for.
Instead, restore a backup to a database of its own, at the version this deployment runs, and take that one
workspace from it:

```bash
ferrum task extract_tenant from:"postgres://ferrum:...@restore-host/ferrum_copy" tenant:acme note:"deleted a ledger by mistake"
```

That compares and changes nothing: it shows, table by table, how many rows there are now and in the copy, and
what went out after the copy was taken (`went_out_since`, by topic: payments taken, messages sent, deliveries
made, as far back as the outbox keeps them, `outbox_keep_days`). Add `apply:yes` to do it. Every erasure the
workspace has made is done again in the same transaction, before anyone sees the copy, so what was erased since
it was taken stays erased without a task of its own.

Tables here refer to each other in circles, so the rows go in with referential triggers off. That takes a grant,
which is worth taking away again afterwards. As a superuser:

```sql
GRANT SET ON PARAMETER session_replication_role TO ferrum;
-- and when it is done
REVOKE SET ON PARAMETER session_replication_role FROM ferrum;
```

It happens in one transaction, with the workspace suspended first so that nobody is writing to it. Every
reference the workspace's rows make is checked before it commits, and one that would point at nothing stops it.
If it stops part way, nothing has changed and the workspace is still suspended: run it again. Other workspaces
are not touched and do not wait, but a workspace of many rows holds its transaction open for as long as it takes.
Numbers issued since, such as invoice numbers, may be issued again.

| Going back does not undo | Because |
|---|---|
| The audit trail, support grants and what support did | They record what happened |
| Each app's records of the same kind, such as Cloud's signing keys, revocations and usage | The same |
| Passwords, second factors and deactivations, as they are now | Nobody gets in who could not before |
| Passkeys removed since | The same |
| Legal holds and the versions kept under them, restrictions of processing, consent given or withdrawn | What the law holds and what the people the data is about asked for |
| Erasures | Each is done again, from the workspace's own record of them (`privacy_erasure`, ids and field names only) |

Everyone has to sign in again and **every API key has to be issued again**, because a revocation since the
copy was taken would otherwise come undone. Roles and everything else are as they were in the copy, and what
was waiting in the copy is set aside as it is after a restore. Workspace settings and quotas stay as they are
now. The files the copy names that the workspace did not are checked against the store afterwards: those whose
bytes went since the copy was taken are listed (`files.missing`, and in the audit trail), since their records came
back without them. What went out since (`went_out_since`) is not undone: reconcile it by hand, such as a payment
the copy does not record or stock that left the warehouse; it is kept in the audit trail, since the outbox goes
back with the copy. A resource made since is no longer known to the workspace though it still exists at its
provider. Look at `cloud_resource_instance` in the live database first if that matters.

## Plans and sign-up

A plan is a name for a set of quotas. The operator defines plans in `FERRUM_PLANS` and puts a workspace on one; its
quotas are then the plan's, and Administration's Workspace tab shows them beside what the workspace uses:

```bash
ferrum task set_plan slug:acme plan:team
```

| Quota | Held where |
|---|---|
| `people` | Turning someone into an active member, by any path (added, invited, SCIM, single sign-on, turned on again): a database trigger refuses one past the quota. Service accounts and people turned off do not count |
| `storage_bytes` | Storing a file |
| `ai_tokens_month` | Each step of the assistant; a workspace may set itself less (`assistant_monthly_tokens`), not more |
| `devices`, `measurements_per_day` | Operations: adding a device, and recording measurements |

A quota left out of a plan has no limit. Moving a workspace to a smaller plan takes nothing away: past a quota, nothing
more of it is added until there is room. Nothing here charges anyone: prices and invoices are the operator's.

With `FERRUM_SIGNUP=open` (and a base domain and mail), anyone with an address at one of `FERRUM_SIGNUP_DOMAINS` can make a
workspace at `accounts.{base}/signup`: a code emailed to the address proves it, then they name the workspace and its address
and choose a password. It installs `FERRUM_SIGNUP_PACKAGES`, starts on `FERRUM_SIGNUP_PLAN`, gets its subdomain, and its
maker is its administrator. Someone with no Ferrum account gets one, signed in; someone with one signs in to it as usual
(a code never opens an account that has a password or a second step of its own), and finds the workspace in their list.
One address makes `FERRUM_SIGNUP_PER_ADDRESS` workspaces at most; the sign-in limits cover the calls.

## Closing and deleting a workspace

A workspace is closed by its operator, or by its administrator (Administration, Workspace, *Close this workspace*, typing
its name: kept 30 days), kept until a date, and deleted after that only when an operator says so. Nothing deletes one by
itself.

```bash
ferrum task account_state slug:acme state:closed retain_until:2027-02-01 note:"the contract ended"
```

Closed is read-only for everyone, like `restricted`: everything can still be read and exported, and nothing new starts.
Apps wind down what they run. Cloud closes each project, stops its sites and keeps what it held. Setting another state
opens it again, as it was.

After the date, an app that made things elsewhere lets go of them first. For Cloud that is
`ferrum task cloud_retire slug:acme apply:yes`, which destroys the databases and repositories Cloud made by the same
durable work a person's deletion plan starts, and never touches something it cannot prove it made. Then:

```bash
ferrum task delete_tenant slug:acme confirm:acme note:"kept until 2027-02-01, as agreed"
ferrum task delete_tenant slug:acme confirm:acme note:"kept until 2027-02-01, as agreed" apply:yes
```

The first says what would go: rows by table, files, legal holds in force, and anything an app still holds outside the
database. A hold, or anything outside, stops it: release each hold first (Administration, Legal holds), which is
recorded. The second deletes the workspace's files, then its private app bundles, then every row that names it, in one
transaction, and records in `tenant_deletion` that it happened. A file store failing part way leaves every row in place,
so running it again finishes the job. Like extraction, it needs `GRANT SET ON PARAMETER session_replication_role TO
ferrum` for as long as it runs.

Backups taken before the deletion still hold the workspace until they expire. Say so to whoever asked for the deletion.

## Working offline

What devices may keep for people to work without a connection (a till's price book; work done
offline until it is sent) follows one policy, set per workspace: [offline.md](offline.md).

## Routine tasks

```bash
ferrum task run_maintenance          # anchor audit trails, purge expired events, ledger partitions, queue notification digests (also runs in-process, hourly)
ferrum task sync_schema              # force the metadata → DDL sync (normally automatic at boot)
ferrum task create_tenant slug:... name:... email:... password:... [packages:...]   # core only without packages:
ferrum task set_plan slug:... plan:...   # put a workspace on one of FERRUM_PLANS (its quotas)
ferrum task extract_tenant from:<url> tenant:<slug> note:"..." [apply:yes]   # take one workspace back to what a restored copy holds
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
"Update" on its Apps page. A release's packages change for every workspace that has them as it is made
available, so the administrators of each are told what it changed there: the customizations it parted
ways with, to keep or switch off under Customizations, and the records waiting in workflow states it
removed or changed. The workspace whose plan brought it saw the same in the plan before agreeing.
