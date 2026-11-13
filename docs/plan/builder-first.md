# Builder first: industries as configuration, not apps

Plan, 10 November 2026. **Status: approved 10 November 2026; in progress.** Decisions in
[docs/builder/decisions.md](../builder/decisions.md); each requirement's state in
[docs/builder/traceability.md](../builder/traceability.md).

It changes the order of the closure program ([docs/registry/feo.yaml](../registry/feo.yaml)): the
industry packages still open stop being closed one app at a time, in Rust inside each app, and the
patterns those apps keep writing become the builder's.

## As revised on 10 November 2026

The same day's review of the whole ecosystem changed the order and the measure. Sections 1 to 9 below
stand as written, except where this section says otherwise.

**The measure.** A business nobody wrote an app for (a gym with memberships), built in the browser by
an administrator, with no code. It is walked at the end of each step and replaces the count of register
rows as the sign of progress. First walk:
[docs/builder/evidence/browser-2026-11-10-gym.md](../builder/evidence/browser-2026-11-10-gym.md).

**The order.** The foundations first, because everything else binds to them:

| Step | What | State |
|---|---|---|
| Foundations | Custom entities with lines, unique keys and exact totals (BF8, ahead of its place); automations that fire on time (BF6); rules that count and sum other records | Built: PRs 158, 160, 159 |
| Authoring | Screens for entities, workflows, automations and rules; a batch with a dry run and `redefine_entity`; a mail preview | Built: PRs 161, 162 and Ferrum-Frontend 67 |
| Packs | One export that carries a whole solution (entities, roles, automations, Studio's items, solutions, chosen reference records) and installs elsewhere, with a plan first (BF9) | Built as a document administrators export and install; signing, uninstalling and the store are not |
| Engines | One working-hours clock, then service levels, checklists and scheduling bound to any entity | Clock built (hours on business calendars, working-time functions, waits in open time, `/api/admin/calendars`); service targets built (`add_target`: due moment and state kept on the record, pauses, rounds, a breach marked once by the minute loop); checklists and scheduling not started |
| Proof | The apps that are configuration and nothing else (Government, Retail, GRC) as packs | Governance and Risk measured ([findings](../builder/evidence/grc-as-a-pack-2026-11-10.md)): of 58 behaviours 33 the same, 16 different, 9 not possible at first; 50 the same, 8 different, none not possible after configured actions, menus, words and lines within lines. Not retired |
| What the paper measurement asks for ([Government and Retail](../builder/evidence/government-retail-on-paper-2026-11-10.md)) | Linked records and their lines in expressions; an effect that appends a line or changes the one a parameter names; aggregate filters on the record's own fields; ERP's steps called from configuration | Built: `links.<field>` and `append_line`/`set_line` (BD-14), `{"doc": "<field>"}` in aggregate filters, `invoke` (BD-13), published lists for a public register (`publish_list`, BD-15; G33). Lines within lines built (a custom line's own lines, two levels deep; BD-06 amended 11 November 2026). A workspace's own lines on a package's entity built (an `x_` table of a custom line entity, one level, rows in `custom_record` under the package's record; BD-06 amended 10 November 2026). Not built: sums per item across records (R15), a role named by data (G13) |
| What the proof asks for | Buttons on records set up without code; an effect that writes the record a link names; `today` in aggregate filters; a pack's shell entries and words | Configured actions, `update_linked` and `today` built (BD-11); a workspace's own menu (`define_module`: tile, module, quick create, assistant notes; BD-12) built, and `InterfaceWord` records travel in packs, which the web pages read for the names they show (Governance and Risk: 94 of 106 texts in Arabic and in Hindi) |

**What changed in the gates below.**

* BF8 came first, and BD-06 was amended as it was built (`custom_key` carries `record_id`; no
  `parent_field`).
* BF6's schedules need no workflow engine: a loop, a table of occurrences and the outbox. With
  `create_document`, a recurring charge is an automation; a recurring-charges engine waits until a
  business needs proration or deposits.
* A rule's aggregates read as the workspace, not as whoever saves (BD-10).
* BF2's public pages, BF3's approvals by several people and BF4's money as contracted capabilities
  keep their places after packs.

**Also built on the way.** Mail bodies from automations and `ferrum.mail` are text, with a template for
formatted mail (PR 156). A Helm chart that renders what `deploy/k8s` renders (PR 155). App upkeep with a
lock per app, migrations as a step of their own, Control out of the default build (PR 157).

**What stays outside.** Infrastructure services (private networks, VPN, virtual machines) are not
Ferrum's: Ferrum talks to open interfaces (S3-compatible storage, an OpenAI-compatible model endpoint,
any PostgreSQL, Vault or OpenBao keys, the Helm chart) and holds no provider's code.

## 1. Why

Between 9 and 10 November six closure packages were worked in their apps' code, three to verified and
three to partial: FEO-045 (Hospitality 0.3.0, Restaurant 0.3.1), FEO-046 (Restaurant 0.3.0), FEO-047
(Education 0.3.1), FEO-048 (Healthcare 0.2.0), FEO-050 (Property 0.8.0), FEO-051 (Nonprofit 0.2.0).
Each was tested and each was right for its app, but they kept writing the same things: three more
public pages with a guest's own link, two more portals, more "someone other than whoever did it"
checks, more policies that move money through ERP, more hand-written reconciliations.

A search of the committed code of the 40 public app repositories ([inventory](../builder/inventory.md),
from `scripts/builder_inventory.py`, each place named) finds the same features written again and again:

| Written in the app itself | Apps |
|---|---|
| A scheduled upkeep loop that does something | 32 |
| Pages or endpoints served by the app's own handler | 29 |
| Report sources of its own | 23 |
| ERP invoices made or read by name | 20 |
| "Someone other than whoever did it" checks | 13 |
| Approval decisions written in the app | 11 |
| A page of the workspace's portal | 9 |
| Mail queued by its own code | 9 |
| A guest's own link, hashed and checked by the app | 8 |
| Journal entries posted by the app | 7 |

Every copy behaves a little differently, is fixed one at a time, and is out of a customer's reach. A
new industry costs a new app, and a customer cannot adapt one without a release. The builder already
holds most of the parts (§2). What it lacks is listed in §3, and the program in §5 closes those gaps
and then proves them by building verticals as configuration.

## 2. What the builder already has

Checked at `origin/main` 2318717.

| Part | Where | What it gives |
|---|---|---|
| Customization of packaged entities | `OverlayOp` in `crates/meta/src/types.rs` (AddField, SetProperty, AddRule, AddOptions, SetEntityProperty, DefineEntity, SetWorkflow, SetPrintTemplate, AddAction, AddSection); `crates/kernel/src/customize.rs`; Frontend `src/app/Customize.tsx` | Fields, properties, rules, options, workflows, print templates, actions and form sections on any entity; kept through upgrades with baselines and the upgrade's impact shown (FEO-056); `export_customizations` and `import_customizations` move them between workspaces |
| Custom entities | `validate_custom_entity` in `crates/meta/src/effective.rs`; stored in `custom_record` | Masters and documents of the workspace's own (`X…`), with **no child tables, unique keys, indexes or secret fields** |
| Workflows | `WorkflowTransition` in `crates/meta/src/types.rs` | States, transitions by role, a CEL guard, `not_creator`, `not_creator_of` (FEO-013); **one acting person per transition** |
| Logic solutions (W0–W8) | `crates/logic/src/{solution,rules,decision,process,calendar}.rs`; kernel `capability.rs`, `process.rs`, `logic.rs`, `authoring.rs`; Studio | Rules on records (derivations in dependency order, checks), decision tables, durable processes (invoke a capability, a person's task, wait for a signal, sleep, cases), tests; drafts, simulation, review by someone else, staged deployment; a release travels between workspaces as a pack |
| The capability gate | `crates/kernel/src/capability.rs` | Every entity's record operations, package actions with contracts, workflow transitions, a notification and an e-mail, invoked through the same kernel path as a person, with effect keys and receipts |
| Automations | `crates/kernel/src/automation.rs`, `schedule.rs` | A record's event or a schedule (relative to a date field, or every day, week or month for the records a filter passes), a condition, actions |
| Business calendars | `logic_calendar`; `crates/logic/src/calendar.rs` | A zone, weekends, holidays and hours of the day per weekday; working time with stopped intervals (`add_working`, `working_between`); kept by Studio and `/api/admin/calendars` |
| Record scoping | `row_scope` in `crates/kernel/src/ctx.rs` | `if_owner` grants matched against `owner_fields` (including a child table's emails, such as `guardians.email`), field conditions and ranges |
| Studio | `crates/studio` (`builder.rs`, `query.rs`, `site.rs`) | Reports, charts, cards, dashboards and spaces as ordinary records, checked on save, computed with the viewer's own permissions; the capability catalog; **signed-in people only, no public endpoints**. Certified definitions live in the Metrics app |
| Forms | ferrum-app-forms | Public or internal forms, 21 question types, CEL conditions, answers checked on the server, responses turned into records of any entity, limits |
| Screens | Frontend `src/app/Screen.tsx` | Apps' sandboxed screens as pages, widgets and panels |

## 3. What stops an industry being configured

1. **Portal people cannot be given access.** Their roles count for nothing by design
   (`crates/kernel/src/auth.rs`, "Portal people and guests reach only the pages apps give them";
   [apps-ui.md](apps-ui.md) §9.1), so every portal is app code.
2. **A public page is only a form.** Availability (slots in hours, capacity by date), a guest's own link
   to see, change or cancel within a window, confirmation and reminder mail, a payment link: each app
   writes them.
3. **Approvals are thin.** One person per transition: no "N of these", no "not whoever did step X", no
   targets in business hours with states that pause the clock, no escalation.
4. **Money and stock steps are not policies.** ERP's documents can be created through record
   operations, but there are no contracts for "issue an invoice", "credit it", "take a payment with a
   deduction", "post a journal under a cost center", "issue stock", with their reversals, and no
   decision tables that drive them (a deposit's share, a refund window, an insurer's coverage).
5. **Rules still need source** for decision tables, rules and structured steps (FEO-055).
6. **Schedules on records are upkeep loops**: "the settings' hours before this date field",
   "every day", "when its invoice is paid".
7. **Reconciliations are code**: setting records beside the books (journal lines by cost center and
   account type) is a hand-written report each time.
8. **Custom entities cannot carry an industry**: no child tables (lines, bills, consents), no unique
   keys, no totals computed in money.
9. **A vertical cannot be packaged.** Customizations, Logic releases, Studio items, forms and roles
   export separately; nothing installs, versions and upgrades them together (FEO-057, FEO-069).

## 4. Principles

* **Configuration runs through the kernel's own paths**: the capability gate, the rules evaluator,
  row scopes, the audit trail. A configured step keeps every guarantee code has (one transaction, a
  refusal that says why, an effect done once).
* **Invariants stay with their owner.** ERP's ledger, stock valuation, taxes by country, payroll,
  devices and document editors stay native code; the builder calls them through capabilities.
* **Every primitive is proven by replacing app code.** A gate is done when an existing app's
  acceptance scenario passes against the configured version, with deliberate breaks failing it.
* **Migration without dual writes.** An app keeps running until its pack reaches parity; then its
  data moves and its code is removed. Nothing is written twice in the meantime.
* **Honest status.** Registry rows say what was run, and what was not.

## 5. The program

Each gate lands like the platform's: Ferrum, then Frontend, then apps; local CI; registry rows with
evidence; a deliberate break for each new invariant; a walk in the browser as ordinary roles.

### BF0. Decisions and inventory

* `docs/registry/bf.yaml` (requirements BF-*, checked by `scripts/registry.py` beside fcp, flc and
  feo) and the inventory of §1 made exact, each duplicate named with its file.
* Decisions: what configuration may hold and never holds; the portal access model; the public page
  model; the pack's format, signing and versioning; how an app gives way to a pack; names (pack,
  app, solution).
* The industry packages still open pause, and are re-scoped (§7).

### BF1. Portal access by configuration

* Roles marked for the portal, which portal people may hold: own-records grants only, matched by
  `owner_fields`, extended to ownership through a link (a report card through its student's
  guardians; a visit through its patient's email), at most two links deep and indexed.
* Generic portal pages from metadata: the records a portal role reaches, the fields it may see (field
  groups left out unless granted), and the actions it may take under their conditions (cancel a
  visit, give a consent).
* Defined in Customize, with a preview as a portal person.
* A workspace's own menu marked for the portal (10 November 2026): its entities a person's portal roles
  reach, as an entry of the portal's home on Studio's page (docs/builder/portal.md, Portal menus).
* **Proof**: Education's family portal and Healthcare's patient portal configured, not coded; their
  test assertions pass against the generic endpoints. Breaks: an owner path ignored, a field group
  shown, an action's condition ignored, another tenant's record reached.

### BF2. Public pages

* Forms grows into public pages bound to an entity: the fields asked, an availability source (slots
  from hours, a length and clashes on a resource; or capacity by date), the guest's own link (hashed,
  expiring, with see, change and cancel inside a window), confirmation and reminder mail templates,
  limits per address and a field only programs fill, a payment link through Commerce when installed.
* **Proof**: Healthcare's booking page and Hospitality's booking page configured; their scenarios pass.
  Breaks: a booked time offered, a link kept in plain, a window ignored, the limit ignored.
* Then it serves Bookings, Education's applications and Government's portal applications.

### BF3. Approvals and independence

* On any transition: approvers by role or by a field, N of them with each decision kept, "not whoever
  created it" or "not whoever did step X", delegation with an end date, escalation.
* State targets in business hours: hours of the day added to business calendars, states that pause
  the clock, a breach recorded once.
* **Proof**: ITSM's change advisory board and incident targets (the work set aside in §7), Hospitality's
  inspection by someone other than the cleaner and Government's appeals, configured rather than coded.

### BF4. Money and stock as capabilities

* Contracts in ERP for issuing a sales or purchase invoice, crediting one, taking a payment with
  deductions, posting a balanced journal under a cost center, issuing and receiving stock, each with
  its reversal, effect keys and refusals.
* Decision tables driving them from a process or a rule.
* **Proof**: Hospitality's deposit policy, Healthcare's insurer share and Nonprofit's restricted
  spending expressed as processes and tables; the books end as their app tests expect.
* **Built (10 November 2026, BD-13)**: buttons and automations call the capability catalog's record
  operations, an app's listed (contracted) actions and workflow transitions with `invoke`, through the
  gate, once per effect key, as the system on the runner's behalf; `results[n]` carries what one effect
  made to the next; `GET /api/admin/capabilities` lists what may be called. So ERP's sales invoice or
  delivery note is created with its lines and submitted, posting through ERP's own code, from
  configuration. **Not built**: contracts on ERP's own actions (crediting an invoice, a return at the
  original cost, a transfer through transit), typed money tables, and decision tables bound to `invoke`.

### BF5. Rules without source (FEO-055)

* Typed editors for rules, decision tables and structured steps; totals computed in money; a lossless
  round trip with the source, which stays for what the editors do not cover.

### BF6. Schedules on records

* Triggers "N hours before a date field", "every day at", and "when a linked document changes"
  (an invoice paid); mail templates with merge fields.
* **Proof**: reminders, "paid when its invoice is settled" and breach sweeps run without app upkeep.

### BF7. Reconciliation in Studio (FEO-065)

* A ledger source (journal lines by dimension and account type), a records-against-books comparison,
  certified metrics bound to dashboards.
* **Proof**: Nonprofit's fund ledger and Property's owner reconciliation as Studio reports.

### BF8. Custom entities that carry an industry

* Child tables, unique keys, numbering, totals, links both ways with packaged entities; or, if that
  costs too much in `custom_record`, Studio generating a real package. The choice is made in BF0.

### BF9. Packs, and the proof

* A pack: entities, fields, forms, workflows, rules, processes, portal and public pages, roles,
  dashboards, seeds and words in each language; versioned, signed, installed from the store,
  upgraded with its impact shown (FEO-056), exported between workspaces.
* **Proof**: a Clinic pack built with BF1–BF8 only, run against Healthcare's scenarios through a
  harness that runs an app's acceptance test against a pack; then a Small Hotel pack against
  Hospitality's. What still needed code is recorded.

## 6. What stays code

ERP's ledger, stock valuation, taxes by country and payroll; Edge's device protocols; the document
editors; Commerce's payment providers; and any rule over many records at once that a record's rules
cannot express. A pack calls them; it does not replace them.

## 7. The closure packages still open

All 57 that are not verified (35 not started, 18 partial, 4 blocked outside software), by where they go.

| Goes to | Packages |
|---|---|
| BF1 portal access | FEO-042 (its unified portal tasks; intranet and DAM stay app work) |
| BF2 public pages | FEO-040 Forms and CMS composition; FEO-032 Bookings payment and calendar conflicts |
| BF3 approvals | FEO-043 ITSM (approvals and targets; CMDB and discovery stay in the app); FEO-013 (independence rules, in part); FEO-029 (approved project changes; resource planning stays); FEO-034 (approver delegation; renewals with BF6) |
| BF4 money and stock | FEO-027 payment lifecycle; FEO-028 subscriptions (with BF6); FEO-010 (ERP's contracts, in part) |
| BF5 rules without source | FEO-055 |
| BF6 schedules | FEO-017 (scheduled notifications, in part) |
| BF7 reconciliation and metrics | FEO-065 certified metrics; FEO-044 GRC evidence and objectives; FEO-072 (exception workbenches; its onboarding with BF9) |
| BF9 packs | FEO-057 process packs; FEO-069 (the pack lifecycle; the SDK itself stays); FEO-045, FEO-047, FEO-051 (what they still lack becomes the packs' proofs) |
| Native code in ERP and HR, unchanged | FEO-019 to 024, FEO-033 |
| App work, unchanged | FEO-031, 035, 037, 039, 041, 053, 054, 063, 066, 067, 068, 070 |
| Platform qualification and operations, unchanged | FEO-009, 011, 016, 058, 073 to 082 |
| Blocked outside software, unchanged | FEO-014, 018, 064, 071 |

A package that goes to a gate is closed by that gate's proof, not by its own app. Its row in
`feo.yaml` is rewritten in BF0 to say so, with the gate as its owner.

ITSM's work started on its branch `feo/service-hours` (uncommitted: settings for business hours,
pauses, change approvals) is put aside; BF3 does it in the platform.

## 8. Risks

* **Opening the portal.** BF1 changes a deliberate rule. Portal roles hold own-records grants only,
  field groups are left out unless granted, and every gate has leakage tests across people and
  tenants. The independent security review stays a blocker (BLOCK-SECURITY-REVIEW).
* **Speed.** Ownership through links must stay indexed and shallow; BF1 measures it on the soak data.
* **Two ways for a while.** Apps and packs coexist until parity, never both writing the same records.
* **Scope.** No gate claims "every industry"; each ends at its proof.

## 9. Order

BF0, then BF1 to BF4 (the largest, each about one Logic gate), then BF5 to BF8, then BF9. BF1 and BF3
do not depend on each other. Each gate is one or more pull requests, merged after local CI at the
tested head, with its rows in `bf.yaml`.
