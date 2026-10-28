# Ferrum

A metadata-driven ERP in Rust on PostgreSQL: ERPNext's good ideas (documents, child tables,
submit/cancel lifecycle, customize-without-code) on foundations that are exact, append-only and
built to scale out.

* **Exact money.** Decimals everywhere (`rust_decimal`, `NUMERIC`); one tax/totals engine
  (`ferrum-calc`) shared by server and browser (WASM), with largest-remainder rounding.
* **Ledgers you can trust.** Append-only journal with a deferred balance check in the database,
  per-company accounting periods that refuse postings once closed, receivables/payables as an
  open-item subledger the database refuses to over-allocate, hash-chained audit trail.
* **Inventory without reposting.** Every stock move is valued once at posting time (moving
  average or FIFO cost layers); balances are locked in a fixed order so concurrent postings can
  neither deadlock nor drive stock negative.
* **Customizable without code, safely.** Custom fields land in JSONB (no `ALTER TABLE`), rules
  are CEL expressions, plus workflows, automations, webhooks, print templates and per-tenant
  packages — all layered over the shipped metadata so upgrades never clobber them.
* **Plugin-centric.** Functionality ships as packages (YAML entities, overlays, roles, seeds,
  SQL). Tenant code runs as sandboxed WebAssembly plugins with fuel and memory limits and no
  ambient capabilities.
* **Multi-tenant and horizontally scalable.** Row-level security on every table, stateless API
  replicas, background work coordinated through the database (no single point of failure).

## Repository layout

| Path | What |
|---|---|
| `crates/calc` | Pure tax/discount/rounding engine (also builds to WASM for the browser) |
| `crates/meta` | Metadata model: packages, entities, overlays, CEL rules, effective schema per tenant |
| `crates/kernel` | Document engine on PostgreSQL: CRUD, lifecycle, permissions, RLS, numbering, audit, outbox, webhooks, automations, print, customization |
| `crates/domain` | ERP engines: ledger, open items, stock valuation, selling/buying flows, stock entries, manufacturing + MRP, India GST, reports |
| `crates/plugins` | WebAssembly plugin host (wasmtime) |
| `packages/*` | Shipped packages: `core`, `accounting`, `inventory`, `selling`, `buying`, `manufacturing`, `india` |
| `server` | HTTP API (Loco/Axum), auth, admin, OpenAPI, ERPNext-compatible `/api/resource` shim, CLI tasks |
| `plugins/examples` | Example plugins |
| `deploy` | Docker Compose and Kubernetes manifests |
| `docs` | [API contract](docs/api.md), [operations runbook](docs/operations.md), [plan and analysis](docs/plan/PLAN.md) |

The web app lives in its own repository, [CBFerrum/Ferrum-Frontend](https://github.com/CBFerrum/Ferrum-Frontend).

## Quick start (development)

Requirements: Rust 1.95+, PostgreSQL 17+, Node 22+.

```bash
createuser ferrum --pwprompt          # password: ferrum
createdb -O ferrum ferrum_dev
```

```bash
cd server && cargo run -- start       # http://127.0.0.1:5150 — migrates and syncs the schema at boot
```

```bash
cd server && cargo run -- task create_tenant slug:demo name:"Demo Ltd" email:admin@demo.test password:'Correct-Horse-9' packages:core,accounting,inventory,selling,buying,manufacturing,india
```

```bash
cd server && cargo run -- task seed_demo tenant:demo
```

```bash
git clone https://github.com/CBFerrum/Ferrum-Frontend web
cd web && npm install && npm run dev  # http://localhost:5173/app/ (proxies the API to the server)
```

Sign in with tenant `demo`, `admin@demo.test` / `Correct-Horse-9`. The server also serves a
production build found in `web/dist` at `/app/` (set `FERRUM_WEB_DIR` elsewhere), which is handy
for single-binary installs.

## How it fits together

```
browser ── /api/v1/{Entity} ──► server (Loco/Axum) ──► kernel ──► PostgreSQL (RLS per tenant)
                                     │                   │
                                     │                   ├─ hooks: domain engines (ledger, stock, …)
                                     │                   └─ hooks: tenant WebAssembly plugins
                                     └─ background loops on every replica: outbox, webhooks,
                                        metadata listener, maintenance (coordinated in the DB)
```

A request resolves the tenant's **effective schema** (shipped packages + installed overlays +
tenant customizations), checks permissions, runs the document pipeline (defaults → links →
`before_validate` hooks → computed fields → metadata rules → `validate` hooks → write → events)
inside one transaction, and records an audit entry and an outbox event in that same transaction.

## Customizing without code

| Need | How |
|---|---|
| Add a field, option, rule or label | `POST /api/customize` with an overlay op (`add_field`, `set_property`, `add_rule`, `add_options`) — stored per tenant, applied instantly, no DDL |
| Validate or derive values | CEL rules/computed fields, e.g. `doc.due_date >= doc.posting_date` |
| Approvals | Workflow overlay (`set_workflow`): states, transitions, roles, CEL guards, maker-checker |
| React to events | Automations (CEL condition → actions) and signed webhooks (HMAC, retries, SSRF guard) |
| Documents | MiniJinja HTML print templates per entity |
| New entity | `define_entity` overlay (stored without DDL) |
| Bundles | Packages: YAML entities, overlays, roles, seeds and SQL, installed per tenant |
| Custom code | WebAssembly plugins bound to entity hooks — see below |

### WebAssembly plugins

A plugin is a core WebAssembly module with **no imports** that exports `memory`,
`ferrum_alloc(len) -> ptr` and `ferrum_hook(ptr, len) -> (ptr << 32 | len)`. It receives the
document as JSON and answers `{}`, `{"patch": {...}}` (in `before_validate`) or a veto
(`{"error": "..."}` / `{"errors": [{"field", "message", "row"?: ["items", 0]}]}`). Each call gets a fresh instance with a
fuel budget and a memory cap; failures abort the operation (fail-closed) and are logged.
`plugins/examples/discount-guard` is a complete Rust example; install with
`POST /api/admin/plugins` (`{name, hooks: [{entity, stage}], wasm_base64}`), toggle with
`PUT /api/admin/plugins/{id}/enabled`.

## Testing

```bash
createdb -O ferrum ferrum_test && createdb -O ferrum ferrum_domain_test
cargo test --workspace                # unit + integration (resets the two test databases)
cd web && npm test
```

`scripts/smoke.py` is a post-deploy check against a running server (readiness, login, reports,
idempotency, optimistic locking, and 60 concurrent stock issues racing for 100 units).

The domain integration test walks a GST-registered company through opening stock,
procure-to-pay, order-to-cash, payments, credit notes, credit limits, manufacturing, MRP, period
closing, concurrent stock issues, sales returns and plugins, and checks that the trial balance and
balance sheet balance and that stock value equals the stock accounts.

## Deployment

* Images: `ghcr.io/cbferrum/ferrum` (API server) and `ghcr.io/cbferrum/ferrum-frontend` (web app,
  which also proxies the API so browsers see one origin). Both are stateless.
* Single host: `deploy/docker-compose.yml`.
* Production: `deploy/k8s` (CloudNativePG with synchronous replica and PITR backups, 3+ API
  replicas, PDB, HPA, NetworkPolicy, probes) — see [docs/operations.md](docs/operations.md).

## Scope and known limits

Included: multi-company accounting (journals, payments, multi-currency with exchange
differences, periods, financial statements), receivables/payables with ageing, selling and buying
cycles with returns, inventory with batches/serials, BOMs, work orders, MRP, India GST (GSTIN,
place of supply, CGST/SGST/IGST templates, HSN checks), reports, customization, plugins.

Not yet included: e-invoice/e-way-bill submission to the GST network (fields only, needs a GSP),
PDF rendering (HTML print only), payroll/HR, CRM, POS, GraphQL, SSO (OIDC), Citus sharding
configuration. Plugins are pure functions today (no host data API).

## Licence

AGPL-3.0-or-later.
