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
| `crates/plugins` | WebAssembly plugin host (wasmtime): hooks, routes, WebSockets, schedules, host calls, fuel and memory limits |
| `crates/apps` | Signed `.fapp` app bundles, manifests, permissions and dependency resolution (shared by server, SDK and store) |
| `crates/mcp` | MCP server: every entity, action and report available to AI agents |
| `crates/procurement` | Material requests, auto-reorder, requests for quotation with supplier links, supplier quotations and scorecards |
| `crates/stock-ops` | Stock reconciliation, landed costs, pick lists, packing slips, quality inspection, item variants, product bundles |
| `crates/finance` | Budgets, bank reconciliation, period closing, payment terms, dunning, subscriptions |
| `crates/mfg-ops` | Workstations, routings, job cards, production planning, subcontracting |
| `crates/studio` | Analytics engine and the no-code report builder, charts, number cards, dashboards and Spaces |
| `packages/*` | Shipped packages: `core`, `accounting`, `inventory`, `selling`, `buying`, `manufacturing`, `india` |
| `server` | HTTP API (Loco/Axum), auth, admin, OpenAPI, ERPNext-compatible `/api/resource` shim, CLI tasks |
| `plugins/sdk`, `plugins/examples` | Rust plugin SDK and example plugins |
| `deploy` | Docker Compose and Kubernetes manifests |
| `docs` | [API contract](docs/api.md), [plugins](docs/plugins.md), [operations runbook](docs/operations.md), [known limits](docs/limits.md), [plan and analysis](docs/plan/PLAN.md) |

The web app lives in its own repository, [CBFerrum/Ferrum-Frontend](https://github.com/CBFerrum/Ferrum-Frontend).

### Apps

Larger business areas are native apps in their own repositories, installed per workspace from
**Apps** (or the app store) like a WordPress plugin; the server image ships them as cargo features.
Third parties build wasm or metadata apps with the [SDK](https://github.com/CBFerrum/ferrum-sdk)
(`ferrum-app new`, `pack`, `sign`, `publish`) — an EV-charging (OCPP) back end, a clinic, a school
or any other vertical is a plugin, not a fork.

| App | Repository |
|---|---|
| CRM | [ferrum-app-crm](https://github.com/CBFerrum/ferrum-app-crm) |
| Projects and timesheets | [ferrum-app-projects](https://github.com/CBFerrum/ferrum-app-projects) |
| Helpdesk | [ferrum-app-helpdesk](https://github.com/CBFerrum/ferrum-app-helpdesk) |
| Point of sale | [ferrum-app-pos](https://github.com/CBFerrum/ferrum-app-pos) |
| Commerce (online store) | [ferrum-app-commerce](https://github.com/CBFerrum/ferrum-app-commerce) |
| HR and payroll | [ferrum-app-hr](https://github.com/CBFerrum/ferrum-app-hr) |
| Mail, chat and calendar | [ferrum-app-comms](https://github.com/CBFerrum/ferrum-app-comms) |
| Fixed assets | [ferrum-app-assets](https://github.com/CBFerrum/ferrum-app-assets) |
| Bookings and appointments | [ferrum-app-bookings](https://github.com/CBFerrum/ferrum-app-bookings) |
| Field service | [ferrum-app-field-service](https://github.com/CBFerrum/ferrum-app-field-service) |
| Forms and surveys | [ferrum-app-forms](https://github.com/CBFerrum/ferrum-app-forms) |
| Learn (courses, quizzes, certificates) | [ferrum-app-lms](https://github.com/CBFerrum/ferrum-app-lms) |
| Drive (files and sharing) | [ferrum-app-drive](https://github.com/CBFerrum/ferrum-app-drive) |
| Campaigns (email marketing) | [ferrum-app-campaigns](https://github.com/CBFerrum/ferrum-app-campaigns) |
| Sign (electronic signatures) | [ferrum-app-sign](https://github.com/CBFerrum/ferrum-app-sign) |

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

## AI agents (MCP)

Ferrum is an MCP server: `POST /mcp` speaks the Model Context Protocol (Streamable HTTP,
stateless, so any replica answers). Agents get generic, metadata-driven tools that cover every
entity, action, workflow and report of every installed package, customizations included:
`ferrum_overview`, `describe_entity` (with a JSON Schema), `search_records`, `get_record`,
`preview_record`, `create_record` (idempotent), `update_record`, `delete_record`,
`submit_record`, `cancel_record`, `amend_record`, `run_action`, `workflow_transition`,
`run_report`, `get_audit_trail` and, for administrators, `customize_entity`. Each call runs as
the signed-in user with that user's permissions and lands in the audit trail; tools carry
read-only/destructive hints so clients can ask before posting.

```bash
claude mcp add --transport http ferrum https://erp.example.com/mcp --header "Authorization: Bearer fk_..."
```

Create the API key under the user menu (or `POST /api/auth/api_keys`).

Hosted assistants (for example a claude.ai custom connector pointed at `https://erp.example.com/mcp`)
use OAuth 2.1 instead: the server advertises itself through the standard discovery documents,
clients register themselves, and the user signs in and approves the client on a consent page.
Access tokens last an hour and refresh tokens rotate; revoking them or signing out everywhere cuts
the client off.

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

### Plugins: turn Ferrum into any platform

Plugins are sandboxed WebAssembly modules with a manifest. Beyond document hooks they can bring
their own entities, serve HTTP routes (signed-in or public), hold WebSocket connections to devices
(with subprotocols such as `ocpp1.6`, delivered to whichever replica holds the socket), subscribe
to events, run on schedules and call declared HTTPS hosts — always within the permissions the
administrator approved, with a CPU budget, a memory cap and no other access. That is how Ferrum
becomes an EV charging, IoT, payment or telephony platform without changing its code. See
[docs/plugins.md](docs/plugins.md), the Rust SDK in `plugins/sdk` and the complete
`plugins/examples/device-gateway`.

## Testing

```bash
for db in ferrum_test ferrum_domain_test ferrum_mcp_test ferrum_plugins_test; do createdb -O ferrum $db; done
cargo test --workspace                # unit + integration (resets the test databases)
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
differences, periods, budgets, bank reconciliation, period closing, financial statements),
receivables/payables with ageing and dunning, subscriptions, selling and buying cycles with
returns in any of an item's units of measure, procurement (material requests, RFQs, supplier
scorecards), inventory with batches/serials, variants, landed costs, pick lists, quality inspection,
BOMs, work orders, job cards, production planning, subcontracting, MRP, India GST (GSTIN, place of
supply, CGST/SGST/IGST templates, HSN checks), analytics (Studio: reports with calculated columns,
subtotals and Excel export, charts, dashboards), collaboration (comments, assignments, tags, followers, chat,
calendar), PDFs and emailing any document, role rules down to records whose fields hold given
values, rate limits that hold across replicas, two-factor sign-in (authenticator apps, recovery
codes, a workspace policy), single sign-on with OpenID Connect providers, live updates, customization, plugins and apps (see above), MCP for AI
agents.

Not yet included: e-invoice/e-way-bill submission to the GST network (fields only, needs a GSP),
GraphQL, Citus sharding configuration. What is deliberately simpler than
you might expect is listed in [docs/limits.md](docs/limits.md).

## Licence

AGPL-3.0-or-later.
