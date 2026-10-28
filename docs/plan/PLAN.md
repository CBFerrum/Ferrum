# Ferrum: End-to-End Plan

**Goal:** re-build ERPNext's idea (a metadata-driven, open ERP) in Rust on PostgreSQL. Make it plugin-first, customizable
without code in a running system, scale-out by design, and correct by construction. Then use it to take share from
SAP, Oracle, Microsoft and Odoo where they are weakest.

## TL;DR

1. **ERPNext's core idea is right; its foundations are not.** 872 DocTypes and ~462k lines of Python sit on:
   * **floating-point money**: `flt()` is the most-connected node in the entire code graph, and a 0.5 debit/credit
     difference is silently absorbed;
   * **mutable ledgers**: cancel-by-flag, delete-and-reinsert on repost, and GL row primary keys renamed by an hourly job;
   * **valuation reposting cascades**: unbounded, non-atomic, mass row locks;
   * **no foreign keys** anywhere;
   * **runtime ALTER TABLE as the customization mechanism**;
   * **last-app-wins plugin overrides**;
   * **~1,550 ad-hoc RPC endpoints**;
   * **≥3.5k lines of tax/totals maths written twice** (Python + JS) that already disagree;
   * **UI-only business rules** that API clients bypass;
   * **PostgreSQL as a regex-translated MySQL dialect**, tested only nightly or on labelled PRs.
2. **Ferrum keeps the good ideas and replaces the foundations:**
   * the DocType concept, docstatus lifecycle, Customize Form, child tables and naming series stay;
   * **Loco** (Axum + SeaORM) + **PostgreSQL 18** (Citus-ready) underneath;
   * **layered metadata with zero-DDL customization**, plus sandboxes and change-sets;
   * **WASM packages** on typed, ordered extension points (no monkey-patching);
   * an **append-only universal journal**, and **cost layers with forward-posted corrections** (reposting can't happen);
   * **Cedar + RLS** authorization;
   * **one Rust calc engine** shared with the browser via WASM;
   * **REST/GraphQL/MCP** generated from metadata, with a **separate React frontend**.
3. **How it beats incumbents:**
   * safe no-code customization that survives upgrades (SAP's "clean core", by construction);
   * fast transactions *and* fast reports in one product;
   * exact ledgers, open Postgres, and per-company pricing (not per user);
   * AI that drafts while deterministic code posts.
   * **Wedge:** ERPNext users who hit the wall → Indian/SEA mid-market manufacturers → electronics/EV hardware (BOM
     revisions, ECO, serial genealogy, EU battery passport from Feb 2027).
4. **Honest limits.**
   * "Infinitely scalable" becomes *scale-out along two axes with stated ceilings* (tenants via Citus; a big tenant via
     partitions, replicas, company sharding).
   * Displacing SAP is a multi-year distribution game. Technology is necessary, not sufficient.
   * ERPNext is **GPL-3.0**, so the licence must be decided before any code is ported.
5. **Next step:** Phase 0, an 8-week kernel spike with a benchmark-based go/no-go gate.

## Documents

| Doc | What's in it |
|---|---|
| [ferrum/README.md](../../README.md) | **The implementation**: kernel, domain engines, plugins, server, web app, deployment ([runbook](../operations.md), [API](../api.md)) |
| [docs/analysis-erpnext.md](analysis-erpnext.md) | How Frappe + ERPNext actually work (v17-dev, cloned 2026-10-28), what to keep, every deficiency with file:line evidence |
| [docs/market.md](market.md) | SAP, Oracle, Microsoft, Odoo, others: what they do well, where they're weak, gaps we can win |
| [docs/architecture.md](architecture.md) | The Rust + PostgreSQL design: metadata kernel, layered customization, WASM packages, ledger/inventory engines, scale-out, robustness |
| `graphify-out/` (generated locally, not in this repo) | Knowledge graph of Frappe + ERPNext (graph.html, GRAPH_REPORT.md, graph.json) |
| `upstream/` (local clones, not in this repo) | Shallow clones: `erpnext`, `frappe`, `rea` |

**How the analysis was done**
* **graphify**, AST mode: 55,861 nodes, 137,711 edges, 3,617 communities over 7,737 code files of Frappe + ERPNext.
* **REA**: static reconstruction of the 439-file desk frontend. It's a global-namespace app with no ES-module graph.
* **Parallel audits**: Frappe internals, ERPNext domain, market (sourced), Rust ecosystem (crates.io-verified).
* **Hand checks**: high-impact claims were re-verified in the source during the session.

Details and evidence: [docs/analysis-erpnext.md](analysis-erpnext.md).

---

## Roadmap

Timelines assume a strong core team with heavy AI-assisted development. ERP breadth is the real cost: Frappe + ERPNext
are ~15 years and ~460k lines of non-test Python. So every phase ends at a usable, sellable product, not a demo.

### Phase 0: Kernel spike (8 weeks, 3–4 people)

Goal: prove the three bets that everything else depends on. (1) Zero-code customization on Postgres without DDL pain.
(2) Rust is materially faster and cheaper to run than Frappe. (3) ERPNext metadata can be imported mechanically.

| Week | Deliverable |
|---|---|
| 1 | Cargo workspace on **Loco**; Postgres 18 (+ Citus image) via docker compose; CI with nextest, clippy, cargo-deny; tenants + auth |
| 2 | `meta`: Entity/Field/View types, JSON Schema, layer merge + patch ops, effective-schema cache with `LISTEN/NOTIFY` invalidation |
| 3 | `schema` + `data`: DDL planner (SeaQuery), `ext` JSONB fields + expression indexes, CRUD for any entity, child-row diff upsert, numbering rules, optimistic locking |
| 4 | Query compiler (filter DSL, link-path joins, keyset pagination); REST + runtime OpenAPI; audit log + outbox |
| 5 | CEL validations/computed fields/`visible_if`; permission model → Cedar + SQL predicates + RLS |
| 6 | React metadata renderer (list, form, line-item grid); "add field / add rule" live from the UI |
| 7 | `erpnext-import` v0: convert ~30 DocTypes (Company, Account, Party, Item, Warehouse, Sales/Purchase Invoice + children) with their customizations. Ledger prototype with property tests |
| 8 | Benchmarks against ERPNext on identical hardware; go/no-go review |

**Benchmark protocol (week 8, the go/no-go gate)**

Same machine (e.g. 16 vCPU / 64 GB), same Postgres 18 for Ferrum. ERPNext 17-dev on MariaDB 11.8 with its default bench
(gunicorn workers + RQ). Data: 50k customers, 20k items, 1M submitted sales invoices × 5 lines.

| Scenario | Measure |
|---|---|
| Create + submit invoice (5 lines, taxes, GL posting) | p50/p95 latency; invoices/s at 50 and 200 concurrent clients |
| Open invoice form (doc + children + metadata) | p95 latency |
| Filtered, sorted list over 1M invoices (customer + date range) | p95 latency |
| Trial balance and AR ageing on 5M journal lines | wall time |
| Add a custom field + validation rule to Sales Invoice | time until live; locks held; downtime |
| Backdated stock receipt with 10k later moves on the same item | time until stock value is correct; rows rewritten |
| Steady-state memory per worker / per 1k concurrent sessions | RSS |

**Exit criteria**
- New entity + fields + validation rule + workflow created live: no code, no restart, no DDL.
- 1M invoices loaded: p95 CRUD < 50 ms, filtered list < 100 ms. Throughput and memory per request ≥ 5× better than ERPNext on the same box.
- Journal invariants hold under `proptest` (balanced entries, no edits, closed-period rejection).
- Imported ERPNext DocTypes render as working forms and lists with no hand-written UI.

### Phase 1: Platform + Finance MVP (months 3–8, ~8–10 people)

- **Platform**: workflows + approvals, automations, report builder, Typst print formats, email, data import/export,
  WASM plugin host v1 (WIT 0.1 + capabilities), packages, sandboxes + change-sets, SSO/MFA, i18n, MCP server.
- **Finance**: chart of accounts, universal journal, AR/AP (invoices, credit notes, payments, advances), bank statement
  import + rule-based reconciliation, tax engine on decision tables, multi-currency + revaluation, period-close
  cockpit, financial statement templates, basic budgets.
- **First localization pack** for the wedge market (e.g. India: GST, e-invoice, e-way bill, TDS).
- **Differential test harness**: the same accounting scenarios on ERPNext and Ferrum, compared line by line.
- **Exit**: 3 design partners running Ferrum accounting in parallel with their live system; parity reports clean.

### Phase 2: Order-to-cash, procure-to-pay, inventory (months 9–14, ~12–15 people)

- Items, UoM, price lists + pricing/discount decision tables; quotation → order → delivery → invoice; RFQ → PO → receipt
  → bill; returns; landed cost.
- Inventory engine: moves, balances, moving-average / FIFO / standard cost, serial/lot genealogy, reservations/ATP,
  forward-posted revaluation (no reposting).
- **ERPNext data migration** (metadata + data + open balances) with a parity report (trial balance and stock value equal
  per period), plus the `/api/resource` compatibility shim.
- GraphQL, webhooks, POS-lite.
- **Exit**: first production go-lives, including at least one company migrated off ERPNext.

### Phase 3: Manufacturing + SaaS scale (months 15–20, ~18–25 people)

- BOM revisions with effectivity, approved manufacturer list, ECO/ECN, work orders/routings/job cards, MRP with pegging,
  subcontracting, quality, standard costing + variances. Projects/timesheets, fixed assets, CRM-lite.
- Marketplace v1 (signed OCI packages), pooled SaaS tier on Citus, offline PWA for warehouse/POS, CDC → analytics.
- **Exit**: 20+ production customers, SaaS live, 10+ third-party packages.

### Phase 4: Enterprise (months 21–36)

Multi-GAAP parallel ledgers, consolidation + intercompany eliminations, finite-capacity scheduling (APS), advanced WMS,
segregation of duties, more localizations (GCC VAT, EU e-invoicing, US sales tax via connectors), SOC 2 Type II /
ISO 27001, partner programme, HR/payroll via partners.

---

## Risks

| Risk | Mitigation |
|---|---|
| ERP breadth overwhelms the team | Wedge market first; parity scoped to ERPNext's core modules; packages let partners fill breadth |
| Accounting/tax correctness | Domain experts (CA/CPA) on staff from Phase 1; property tests; differential tests vs ERPNext; auditor review before GA |
| JSONB customization slower than columns | Benchmarked in Phase 0; expression indexes; promote hot fields to columns in dedicated tenants |
| WASM component-model churn | Pin wasmtime LTS; keep WIT small and versioned; first-party packages are native Rust |
| Loco lock-in or immaturity | Loco is a thin layer over Axum + SeaORM; any piece can drop to plain Axum/sqlx without a rewrite |
| Citus operational complexity | Start on single-node Postgres; schema is Citus-ready so sharding is a deployment change, not a rewrite |
| GPL contamination (ERPNext is GPL-3.0) | Decide licence up front (see below). Clean-room process if not GPL-compatible |
| Adoption against incumbents | Open-source core, ERPNext importer + compat API, partner programme, measurable TCO/perf claims |
| Frappe catches up (v17-dev already adds MCP, an automation engine, a Typst emitter, a Postgres push and a stock-ledger rebuild) | Compete on what Frappe can't ship in a release without breaking its installed base: DB integrity, decimal money, immutable ledgers, sandboxed plugins, zero-DDL customization, scale-out |

---

## Decisions needed from you

1. **Licence.** ERPNext is **GPL-3.0** (Frappe is MIT). Translating ERPNext's code into Rust makes Ferrum a derivative
   work. Options:
   - **(a) AGPL-3.0/GPL-3.0**: port domain logic directly, fastest path, community-friendly. Monetize via cloud,
     support and enterprise packages.
   - **(b) Apache-2.0/BSL/proprietary**: allowed only with a **clean-room** process: a spec team reads ERPNext and writes
     functional specs, and a separate implementation team never reads ERPNext domain code. Slower.
2. **Wedge market**: e.g. Indian mid-market manufacturers/distributors (incl. electronics/EV hardware), or "ERPNext
   users who've hit the scaling wall", or another vertical you know well.
3. **First deployment model**: self-hosted single company (simplest, matches ERPNext users) or multi-tenant SaaS first.
4. **Frontend**: React (recommended: ecosystem + hiring) or Vue (closer to frappe-ui). Rust UI frameworks
   (Leptos/Dioxus) are still pre-1.0, so they're only for small embedded pieces. The shared calc engine ships to either UI as WASM.
5. **Name**: "Ferrum" is a placeholder.
