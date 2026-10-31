# Ferrum apps: one format, one store, three runtimes

Everything beyond the platform — CRM, Helpdesk, HR & Payroll, POS, Mail, Commerce, and every
third-party or vertical extension (EV charging, payment gateways, e-invoicing...) — is an **app**:
discovered in a store, installed per workspace with a permissions review, upgraded, disabled and
removed without touching Ferrum itself.

## 1. Decision

**Yes to an app ecosystem; no to "everything is an unsandboxed plugin".** WordPress plugins are
PHP loaded into each site's own process with full privileges — fine for one site per server, and
still WordPress's main source of compromises. Ferrum is multi-tenant: one process serves many
workspaces, so an uploaded native plugin could read every workspace's data. Mature multi-tenant
platforms solve this the same way — Salesforce (metadata + sandboxed Apex), Shopify (apps +
WebAssembly Functions), Odoo Online (official modules compiled in, custom code only on dedicated
hosting). Ferrum does the same with three runtimes behind one format:

| Runtime | Code | Installed from | Who | Can do |
|---|---|---|---|---|
| **native** | Rust crate compiled into a Ferrum distribution | the store (enabling code already in the image) | Ferrum's own heavy apps; self-hosters' custom builds | everything the kernel offers: real tables, hooks inside the document transaction, direct SQL, raw sockets (SMTP/IMAP), web endpoints, reports, upkeep |
| **wasm** | WebAssembly module (sandboxed: fuel, memory, declared permissions only) + metadata | the store, or uploaded | third parties, verticals, integrations | hooks, actions, HTTP routes, WebSockets, events, schedules, HTTPS to declared hosts, entities, settings |
| **meta** | none: metadata only | the store, or uploaded | industry kits, templates | entities, fields on existing entities, roles, menus, workflows, print formats, automations, seed data |

Why the heavy apps stay native: SMTP/IMAP need raw sockets (the sandbox speaks HTTPS only);
payroll, POS consolidation and ledger postings need direct transactional SQL and speed; and they
are first-party, reviewed code. They are still apps — separate repositories, own versions,
installed per workspace from the store — their code simply ships in the server image, like Odoo's
official modules.

## 2. Concepts

* **App** — the unit people install. Identified by `id` (`[a-z][a-z0-9-]{1,39}`), versioned with
  semver, published by a **publisher**.
* **Package** — Ferrum's existing metadata unit (`package.yaml`, `entities/`, `sql/`, `print/`). An
  app provides one or more packages.
* **Release** — one version of an app: its manifest, packages, optional `plugin.wasm`, assets,
  signatures.
* **Bundle** — a release as a file: `<id>-<version>.fapp`.
* **Store** — a registry of releases (the official one, a company's private one, or both).
* **Availability** (cluster) vs **installation** (workspace): a release is made *available* on a
  cluster once (its packages join the catalog and their tables are created); each workspace then
  *installs* it (its packages become part of that workspace's schema, its plugin is enabled, its
  roles and menus appear).

## 3. Manifest (`app.yaml`)

```yaml
id: ev-charging                 # unique in the store
name: EV Charging
version: 1.4.0                  # semver
publisher: voltworks            # store publisher id
summary: Run charge points over OCPP 1.6J and roam over OCPI 2.2
description: README.md          # file in the bundle (markdown)
categories: [energy, iot]
icon: assets/icon.svg
screenshots: [assets/stations.png]
license: Apache-2.0
homepage: https://voltworks.example
support: help@voltworks.example
runtime: wasm                   # native | wasm | meta
platform: ">=0.3, <0.4"         # Ferrum app API versions it works with
requires:                       # other apps, semver requirements
  selling: ">=0.3"
packages: [ev_charging]         # directories under packages/
plugin: plugin.json             # wasm only: the plugin manifest (hooks, routes, sockets, ...)
prefix: Ev                      # third-party entities must start with it (reserved per publisher)
settings:                       # shown on the app's settings page; secrets are write-only
  - {key: ocpi_token, label: OCPI token, type: secret}
uninstall: keep                 # keep | ask — whether removing offers to delete the app's data
```

Rules enforced on publish and install: `id`, `version`, `runtime`, `platform` required; packages
must exist in the bundle and validate against the platform's metadata schema (the same validator
Ferrum boots with); third-party entity names start with `prefix`; a wasm app's plugin manifest is
validated like an uploaded plugin; meta apps carry no code and no `sql/`; `sql/` in packages is
allowed only for **verified** publishers (it runs with the application role, still under row-level
tenant isolation).

## 4. Bundle format

* gzip-compressed tar, files at the root: `app.yaml`, `README.md`, `CHANGELOG.md`, `LICENSE`,
  `packages/<name>/...`, `plugin.wasm` + `plugin.json` (wasm apps), `assets/...`,
  `SIGNATURES.json`.
* Limits: 50 MB unpacked, 2 000 files, 8 MB wasm, 2 MB per text file; only regular files; no
  absolute paths, `..`, links or devices; UTF-8 for text.
* **Digest**: SHA-256 over `ferrum-app-v1\n` followed by one line per file except
  `SIGNATURES.json`, sorted by path: `<path>\0<sha256-hex of content>\n`. Identical content →
  identical digest, regardless of tar ordering or timestamps.
* **Signatures**: `SIGNATURES.json` = `{"digest": "<hex>", "signatures": [{"key_id": "<hex of
  first 8 bytes of SHA-256(public key)>", "role": "publisher" | "store", "sig": "<base64 Ed25519
  over the digest bytes>"}]}`.

## 5. Trust and security

* **Signing.** Publishers register Ed25519 keys with the store and sign releases; the store
  countersigns after review. A Ferrum cluster trusts the store keys it is configured with
  (`apps.trusted_keys`) and refuses bundles whose digest or signatures do not verify. Key rotation:
  several keys may be trusted at once.
* **Review.** Automatic checks on submission (manifest, metadata validation, wasm validation,
  size, permission diff against the previous version, known-vulnerable dependencies) and a human
  review before the store signs; a "verified publisher" tier unlocks `sql/`.
* **Permissions review.** Installing shows exactly what the app asks for (records it reads and
  writes, hosts it calls, public endpoints, sockets, schedules); the administrator's consent is
  recorded in the audit trail with the permission digest. An upgrade that asks for more stops for
  a new consent.
* **Runtime enforcement.** wasm apps are limited to their declared permissions at every host call
  (fuel, memory, host-call and output limits, no ambient I/O). meta apps have no code. Native apps
  are trusted code reviewed with Ferrum.
* **Revocation.** The store publishes a signed revocation list; clusters poll it and disable a
  revoked release everywhere (data kept), like browser extension blocklists.
* **Private apps.** Workspace administrators may upload unsigned wasm/meta bundles when the
  cluster allows it (`apps.allow_uploads`); private apps cannot create tables (their entities live
  in the generic custom-record store) and are visible only in that workspace. Self-hosters who own
  the whole database can allow private tables (`apps.allow_private_tables`).

## 6. Lifecycle

* **Make available (cluster).** From the store (download → verify → cache in object storage) or a
  trusted upload. Packages join the cluster catalog; the kernel creates their tables and columns
  (additive DDL under the cluster DDL lock); every replica reloads its catalog (NOTIFY). One
  version of an app per cluster — tables are shared by all workspaces.
* **Install (workspace).** Resolve `requires` (installing missing dependencies first), check
  `platform`, show permissions, record consent, install the packages into the workspace (seed
  data, roles, menus), enable the plugin, apply settings.
* **Disable / enable (workspace).** Hooks, routes, schedules and menus stop; data stays.
* **Upgrade (cluster, then every workspace).** New release made available (additive DDL), declared
  migrations run, plugin switched. Destructive changes (drop/rename a field) are explicit
  migration steps, never implicit. Workspaces keep their data; permission increases wait for
  consent.
* **Uninstall (workspace).** Removes the packages from the workspace schema, the plugin and its
  private data; with "delete data" (when the app allows it) also deletes the workspace's rows in
  the app's tables. Apps other installed apps require cannot be uninstalled first.

## 7. Storage (stateless)

Cluster tables: `app_release` (id, version, runtime, manifest, digest, signatures, bundle object
key, packages as files, plugin module digest, status), `app_revocation`. Workspace table:
`tenant_app` (app, version, enabled, consent digest, installed by/at). Bundles live in object
storage (S3); every replica can rebuild its catalog from the database alone.

## 8. App Manager API (workspace administrators)

| | |
|---|---|
| `GET /api/admin/apps` | installed apps, available updates, native apps in this build |
| `GET /api/admin/apps/store?q=&category=` | store search (through the server: verified, cached) |
| `GET /api/admin/apps/store/{id}` | app page: description, versions, permissions, compatibility |
| `POST /api/admin/apps/{id}/install` `{version?, consent}` | install (with dependencies) |
| `POST /api/admin/apps/{id}/enable` / `disable` | |
| `POST /api/admin/apps/{id}/upgrade` `{version, consent?}` | |
| `DELETE /api/admin/apps/{id}?delete_data=true` | uninstall |
| `POST /api/admin/apps/upload` | private app bundle (`.fapp`) |

The web app's **Apps** page offers Installed / Store / Upload; AI agents get the same operations
over MCP (administrators only).

## 9. The store (`ferrum-store`)

A stateless Rust service (PostgreSQL + S3) with:
* **Public API**: `GET /v1/apps?q=&category=&platform=`, `GET /v1/apps/{id}`,
  `GET /v1/apps/{id}/releases`, `GET /v1/apps/{id}/releases/{version}/bundle` (redirect to a
  signed object URL), `GET /v1/revocations` (signed), `GET /v1/index` (signed snapshot for
  mirrors and air-gapped installs).
* **Publisher API** (token): register keys, submit a release (bundle upload), see review status,
  yank a release.
* **Review**: automatic checks, moderator queue, approve (countersign) / reject with reasons,
  verified-publisher tier, revocations.
* **Storefront**: public web pages for browsing apps (description, screenshots, versions,
  permissions, compatibility, ratings and reviews from installed workspaces).
* **Commerce-ready**: free and paid apps; paid apps get a store-signed license token per cluster
  or workspace, checked at install and upgrade.
* Organisations can run their own store (same software) and configure several stores, like
  package sources.

## 10. SDK and CLI (`ferrum-sdk`)

`ferrum-app new <id> --runtime wasm|meta|native` (scaffold), `check` (validate manifest, packages,
plugin), `dev` (upload as a private app to a local server and re-install on change), `test`,
`pack` (deterministic `.fapp`), `keygen`, `sign`, `publish`. Also the wasm guest SDK
(`ferrum-plugin-sdk`), JSON Schemas for `app.yaml` and package YAML (editor validation),
templates, CI workflow templates and the developer guide.

## 11. Native apps in their own repositories

* Each native app repository holds a Rust crate (`ferrum-app-<id>`) and its `packages/`. The crate
  depends on the platform crates (`ferrum-kernel`, `ferrum-meta`, `ferrum-domain`; an app that builds
  on ERP's documents also on `ferrum-app-erp`, erp-repo.md) by git tag and
  embeds its packages at compile time, exposing one entry point (manifest, embedded package files,
  hooks, reports, endpoints, upkeep).
* The Ferrum server includes official native apps as cargo features pointing at those
  repositories (pinned tags), with `[patch]` unifying the platform crates; a build without an app
  refuses to start while that app's packages are in use.
* Each app's CI tests against the platform version it targets; the platform's CI builds the
  official image with every official app.
* Self-hosters build custom images with any set of native apps.

## 12. Repositories

| Repository | Contents |
|---|---|
| `CBFerrum/Ferrum` | platform: kernel, metadata, ERP core (accounting, inventory, selling, buying, manufacturing), localizations, plugin sandbox, MCP, App Manager, server |
| `CBFerrum/Ferrum-Frontend` | the web app (including the Apps page) |
| `CBFerrum/ferrum-sdk` | `ferrum-app` CLI, wasm guest SDK, schemas, templates, developer guide |
| `CBFerrum/ferrum-store` | store service and storefront |
| `CBFerrum/ferrum-app-crm`, `-projects`, `-helpdesk`, `-hr`, `-pos`, `-comms`, `-commerce` | official native apps |
| later: `ferrum-app-bookings`, `-forms`, `-sign`, `-drive`, `-lms`, `-field-service`, `-campaigns`, `-assets`, `-ev-charging` | further apps (wasm or native as each needs) |
