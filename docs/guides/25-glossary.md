# 25. Glossary

The words of the platform a developer meets, in alphabetical order, each in a sentence or two with where it is
explained. Where a word names code, the code's name is given.

**Acceptance scenario.** One of the 58 scenarios for the interface of the whole ecosystem in Ferrum-Frontend's
`docs/design/acceptance.json` (UX-001 to UX-058), each with an exercise, an expected result and the evidence it
needs; all read `not_run`. [17](17-the-interface.md), [18](18-accessibility.md)

**Action.** An operation on a record beyond saving, declared on an entity and called at
`POST /api/v1/{Entity}/{id}/actions/{action}`; the assistant, MCP, bulk operations and the builder can call it too.
[06](06-events-jobs-and-logic.md), [21](21-apis-and-docs.md)

**Add-on.** An app with `parent:` another app, in the parent's repository under `addons/`, that adds to it: a
country's rules to ERP (`addons/india`), a warehouse. [01](01-what-an-app-is.md)

**Amend.** Making a new draft from a cancelled submitted document, the way to change what was submitted.
[04](04-data-and-records.md)

**App.** A unit people install in a workspace: a manifest (`app.yaml`), packages of metadata, and for a native app a
Rust crate and often an interface, in its own repository `ferrum-app-<name>`. [01](01-what-an-app-is.md)

**App contract.** What every app gives, in `docs/app-contract.md`: the parts `scripts/check_apps.py` checks and the
parts a person reviews. [README](README.md), [24](24-review-checklist.md)

**`app_upgrading`.** The `503` an app's API answers while its upgrade migrates a workspace; the client waits and
retries. [21](21-apis-and-docs.md)

**Approval.** A policy that holds a workflow transition or an action until named people decide.
[05](05-permissions-and-people.md)

**Assistant.** The platform's AI helper beside a page (`shell().assistant(context)`), which acts through the same
actions and rights as the person. [06](06-events-jobs-and-logic.md)

**At least once.** How events, jobs and saga steps are delivered: possibly more than once, never exactly once, so
every consumer is idempotent. [06](06-events-jobs-and-logic.md)

**Audit trail.** The hash-chained record of every change and who made it, appended only. [05](05-permissions-and-people.md)

**Bar.** `<ferrum-bar>`, the platform's top bar drawn by the shell runtime on every app's page: launcher, search,
Create, notifications, assistant, account, upload tray. [17](17-the-interface.md)

**Baseline (UI lint).** `ui-lint-baseline.json` in an app's `ui/`: how many findings of each lint rule a file may
still have; it only goes down. [17](17-the-interface.md)

**Bootstrap.** The JSON the server puts in every app page as `#ferrum-bootstrap`: the workspace, the person, the
app's routes and release, the locale, the theme, the offline lease. Read with `bootstrap()`. [17](17-the-interface.md)

**Builder.** The program that makes industries configuration rather than code (`docs/builder/`), binding the
platform's engines to any entity. [06](06-events-jobs-and-logic.md)

**Capability (logic).** A business operation offered through one gate that checks rights and records the call
(`docs/logic/capabilities.md`). Not the same as a shell capability (`shell().supports(...)`).
[06](06-events-jobs-and-logic.md)

**Carry.** The vault verb that puts a secret on an outbound request for the hosts its rule names, so the app's code
never holds it. [08](08-secrets-and-the-vault.md)

**Catalog.** An app's translations, English to a language's words, in `ui/src/locales/<language>.json` (pages) and
`ui/src/locales/names/<language>.json` (names its model gives pages). [19](19-languages-and-formats.md)

**Child table.** A field of type table: rows that belong to a record (an invoice's lines), saved with it.
[04](04-data-and-records.md)

**Companion.** Two things. A *service*: a program an app runs beside the server, whose files the server renders
from the database and the vault (`HookRegistry::add_companion`). A *companion's sign-in*: a program people sign in
to through the workspace with OpenID Connect (`HookRegistry::openid_companion`). One program can be both.
[10](10-companions-and-sign-in.md)

**`compiled_apps_fit_together`.** The server's check (`server/src/packages.rs`) that the apps compiled in satisfy each
other's `requires` ranges. [03](03-the-manifest.md), [16](16-releasing.md)

**Connection.** A workspace's configured link to an outside system, with its credential in the vault, used through
the kernel's outbound client. [11](11-integrations.md)

**Connector.** A provider a workspace can connect to (`HookRegistry::add_connector`), or an app that joins two apps'
data (CRM's `crm_erp` package). [11](11-integrations.md)

**`contributes`.** The manifest's list of what an app adds to the shell and other apps: `shell.app`, `shell.open`,
`shell.create`, `shell.intent`, `shell.sidepanel`, `shell.menu`, `shell.portal`, `shell.public`, and more.
[03](03-the-manifest.md), [17](17-the-interface.md)

**Custom field.** A field a workspace adds to an entity by configuration, shown everywhere without code.
[04](04-data-and-records.md)

**`define_module`.** The customization that gives a workspace's own entities a menu and a launcher tile at
`/data/{menu}/`, without a manifest. [17](17-the-interface.md)

**Density.** Comfortable or compact, set per person (`data-density`), changing control heights; touch is always
44px. [17](17-the-interface.md)

**Docstatus.** A submittable record's state: 0 draft, 1 submitted, 2 cancelled. Submitted records do not change.
[04](04-data-and-records.md)

**Doctor.** `ferrum task doctor`: the operator's check of an installation, which names what is wrong.
[14](14-operations-for-apps.md)

**Entity.** A record type declared in a package (`entities/*.yaml`), with fields, rules, actions and permissions; its
table is `e_<name>`. [04](04-data-and-records.md)

**Event.** A row in the outbox written in the transaction that made a change (`{Entity}.created`, `ferrum.*`),
handled after commit by any replica. [06](06-events-jobs-and-logic.md)

**Event sink.** An app's handler of events (`HookRegistry::add_event_sink`). [06](06-events-jobs-and-logic.md)

**Evidence classes.** Source review, component test, real-browser capture, human review: kept apart, and a claim
names which it rests on (Ferrum-Frontend `docs/design/EVIDENCE.md`). [20](20-writing-for-people.md)

**Family.** A group of apps that share a navigation (Drive with Sheets, Slides and PDF); `FamilyNav` in the kit.
[17](17-the-interface.md)

**Field group.** A set of sensitive fields with their own permission, masked or hidden for others.
[05](05-permissions-and-people.md)

**File kind.** A kind of file an app keeps for its own work (`h.file_kind("backup")` → `cloud.backup`), reached only
through the app's handle. [09](09-files-and-storage.md)

**`formatMoment`.** The client's function that shows an instant on the one clock (the person's display zone, else
the workspace's), naming the zone when it is not the browser's. [19](19-languages-and-formats.md)

**Guest.** A person outside the workspace with limited access; keeps nothing offline. [05](05-permissions-and-people.md)

**`HookRegistry`.** The kernel's registry an app registers into at start: `app(id, packages, |h| …)`, and in it its
vault, sites, jobs, event sinks, companions, file kinds. [01](01-what-an-app-is.md)

**Idempotency key.** A value a caller sends (`Idempotency-Key`) so a repeated request is done once and answered the
same. [21](21-apis-and-docs.md), [06](06-events-jobs-and-logic.md)

**`If-Match`.** The header that sends the version a record was read at; a newer one makes the write a `409`.
[21](21-apis-and-docs.md)

**Installation.** One deployment of the server and its database, holding many workspaces; the vault's
`Scope::Installation`. [07](07-stateless-apps.md)

**Intent.** A request one app makes of another through the shell ("pick a file", "compose a mail"), answered by
whichever installed app handles it. [17](17-the-interface.md)

**`InterfaceWord`.** A workspace's own word for an English word of the interface in a language, set under
Administration; it stands over every catalog. [19](19-languages-and-formats.md)

**`isolate()`.** The client's function that keeps text shown as it is in its own direction on a right-to-left page.
[19](19-languages-and-formats.md)

**Job.** Background work of a kind `{app}.{what}` (`HookRegistry::add_job_handler`), retried with backoff.
[06](06-events-jobs-and-logic.md)

**Kit.** `@ferrum/kit`, Ferrum-Frontend's components and tokens every app's interface uses; `@ferrum/kit/primitives`
are its shadcn building blocks. [17](17-the-interface.md)

**Lease (offline).** How long a device may keep cached reads for a person, from the workspace's offline policy, in
the bootstrap's `offline`. [17](17-the-interface.md)

**Link.** A field that points at another record by id. [04](04-data-and-records.md)

**Live channel.** The bar's one WebSocket per browser carrying changes (ids only); `useLiveEntity`, `useLivePush`.
[17](17-the-interface.md)

**Localization seam.** ERP's place for a country's rules (`ferrum_app_erp::localization`), filled by an add-on.
[19](19-languages-and-formats.md), [11](11-integrations.md)

**Manifest.** `app.yaml`: an app's id, version, ranges, settings, interface and contributions.
[03](03-the-manifest.md)

**MCP.** The Model Context Protocol endpoint (`/mcp`) through which outside AI agents use the workspace with OAuth.
[06](06-events-jobs-and-logic.md)

**Member.** A person of the workspace (bootstrap `person.kind: "member"`), as opposed to a guest, a portal person or a
service user. [05](05-permissions-and-people.md)

**Module.** A group of entities and reports in a package's left menu (`modules:` in `package.yaml`, with `icon:`).
[17](17-the-interface.md)

**Move.** Taking a workspace, or a whole installation, to another database or region. [12](12-privacy-and-data-duties.md),
[14](14-operations-for-apps.md)

**Mutant.** A deliberate break of an invariant that a test must catch (`scripts/mutants.py`). [15](15-testing-and-ci.md)

**Mutation path.** A raw SQL write to an entity table outside the document pipeline, each listed with its kind in
`docs/registry/mutation-paths.md` (`scripts/mutation_paths.py`). [04](04-data-and-records.md)

**Native app.** An app whose Rust crate is compiled into the server: real tables, hooks in the document transaction,
endpoints. The other runtimes are `wasm` (plugins) and `meta` (metadata only). [01](01-what-an-app-is.md)

**One input.** The rule that a replica starts from the database address (`DATABASE_URL`) alone; checked by
`scripts/one_input.sh`. [07](07-stateless-apps.md)

**Operation (id).** The id every error carries (`operation`) and an internal error's message names, so an
administrator finds what one request set off. [20](20-writing-for-people.md)

**Operator setting.** A setting an installation's operator chooses, declared in `operator_settings`, kept as
`<app>.<key>`, read with `ferrum_kernel::env::var`. [07](07-stateless-apps.md)

**Outbox.** The table of events, written in the transaction of the change. [06](06-events-jobs-and-logic.md)

**Overlay.** A package's changes to another package's entities (`overlays:`, `add_field`, `add_rule`).
[04](04-data-and-records.md)

**Package.** A unit of metadata in an app: entities, modules, roles, workflows, reports, print formats, SQL
(`packages/<name>/package.yaml`). [04](04-data-and-records.md)

**Pin.** The commit of an app the platform's `Cargo.lock` builds; "the pin is the lock". [16](16-releasing.md),
[24](24-review-checklist.md)

**Plugin.** A WebAssembly app or extension, run sandboxed with fuel and a memory cap, with declared hosts and routes
(`docs/plugins.md`). [01](01-what-an-app-is.md), [13](13-security-rules.md)

**Pooler.** A connection pooler in transaction mode in front of PostgreSQL; supported, so no session state.
[07](07-stateless-apps.md)

**Portal.** The pages for people outside the workspace (customers, suppliers) at `/portal/`; a portal person signs in
with a code and sees only what their address belongs to. [05](05-permissions-and-people.md)

**Process (durable).** A long-running run of steps that waits for people and events and recovers
(`docs/logic/processes.md`). [06](06-events-jobs-and-logic.md)

**R1, R2.** The two releases of the stateless program: through R1 every environment variable still works and wins;
after R2 a program reads `DATABASE_URL` only. [07](07-stateless-apps.md)

**Records frame.** `@ferrum/records/frame`: the sidebar of an app's modules with the generic lists, forms and reports
of its entities. [17](17-the-interface.md)

**Refusal.** A request the platform will not do, answered with a stable `error` code and a message for people.
[21](21-apis-and-docs.md), [20](20-writing-for-people.md)

**Registry (requirements).** `docs/registry/`, the requirements and their states, checked by `scripts/registry.py`.
[16](16-releasing.md)

**Registry (settings).** `server/src/settings.yaml` and the apps' `operator_settings`: every setting with one owner,
checked by `scripts/settings.py`. [07](07-stateless-apps.md)

**Release (interface).** A built `ui/dist` stored under its digest and served at `/_ui/{app}/{digest}/`.
[17](17-the-interface.md)

**`requires`.** The manifest's ranges of other apps an app needs. [03](03-the-manifest.md)

**Reveal.** The vault verb that returns a secret's value, only where its rule allows. [08](08-secrets-and-the-vault.md)

**Route.** The first path segment an app's interface owns (`/crm/`), from `shell.app`; read with `routeBase()`.
[17](17-the-interface.md)

**Row-level security.** PostgreSQL's per-row policy, forced on every tenant table so a transaction sees one
workspace. [04](04-data-and-records.md), [13](13-security-rules.md)

**Rule (vault).** What a secret allows: reveal or not, hosts it may go to, what a sandbox has, the overlap of the
version before. [08](08-secrets-and-the-vault.md)

**Saga.** A durable chain of steps across systems, each with an undo (`ferrum_kernel::saga`). [06](06-events-jobs-and-logic.md)

**Sandbox (workspace).** A copy of a workspace to try things in, without the original's secrets where its rules say
so. [12](12-privacy-and-data-duties.md)

**Scenario test.** A test under `tests/*_it.rs` that runs an app's flows against a real PostgreSQL; the contract asks
for one. [15](15-testing-and-ci.md)

**Scope (vault).** `Scope::Workspace(id)` or `Scope::Installation`: where a secret lives. [08](08-secrets-and-the-vault.md)

**Seal.** The vault verb that encrypts data under the app's own key for a context, to keep anywhere. [08](08-secrets-and-the-vault.md)

**Shell.** The platform's part of every page: the bar runtime and `ferrum.shell`, reached through `shell()` from
`@ferrum/client`, with fallbacks where none runs. [17](17-the-interface.md)

**Side panel.** A panel an app contributes (`shell.sidepanel`) that opens beside any app's page. [17](17-the-interface.md)

**Site.** An app's HTTP endpoints (`trait Site`, `HookRegistry::add_site`), at `/api/app/{package}/…` signed in and
`/api/site/{tenant}/{package}/…` public. [21](21-apis-and-docs.md)

**`StatePanel`.** The kit's component for a region with nothing to show: loading, empty, no matches, denied, offline,
failed. [17](17-the-interface.md)

**Status tone.** A badge's colour, taken from a stable state id (`StatusBadge value=`), never from its words.
[17](17-the-interface.md)

**Step-up.** Asking the person to confirm it is them before sensitive work (`403 step_up`); the client and the kit's
`ConfirmHost` handle it. [05](05-permissions-and-people.md)

**Studio.** The interface for building reports, charts, dashboards, spaces and logic without code (`/studio/`).
[06](06-events-jobs-and-logic.md)

**Submittable.** An entity whose records are submitted (and then cancelled or amended) rather than edited forever.
[04](04-data-and-records.md)

**Support access.** A workspace's grant that lets support reach it, on the record (`docs/support-access.md`).
[05](05-permissions-and-people.md)

**`t()`.** The client's function for every word of the interface, with `{places}`; its argument is a double-quoted
literal. [19](19-languages-and-formats.md)

**Tenant.** The code's name for a workspace (`tenant_id`). [04](04-data-and-records.md)

**Token (design).** A named value in Ferrum-Frontend's `src/index.css` (`--surface`, `--text`, `--primary`,
`--control-h`), the only source of colours and sizes. [17](17-the-interface.md)

**Type role.** One of the text sizes `text-xs` to `text-2xl`, each for a purpose. [17](17-the-interface.md)

**UI lint.** Ferrum-Frontend's `packages/build/uilint.mjs`: raw colours, small text, arbitrary sizes, blank
fallbacks, zoom locks, forked primitives. [17](17-the-interface.md)

**`ui_outdated`.** The `409` an app answers a call it cannot serve for an old interface; the kit offers to reload.
[21](21-apis-and-docs.md)

**Vault.** The platform's store of keys and secrets, sealed in the server; an app reaches only its own through its
handle (`h.vault()`). [08](08-secrets-and-the-vault.md)

**Walk.** Using the changed flows on a running server, as each role, before a release, and recording what was found.
[24](24-review-checklist.md)

**Work home.** An app's own home page answering "what needs me now?", made of the kit's `PageHead`, queues, tiles and
stats. [17](17-the-interface.md)

**Workflow.** States and transitions on an entity, each transition naming who may take it. [05](05-permissions-and-people.md)

**Workspace.** One customer's data and people within an installation (a tenant), at its own host or path.
[01](01-what-an-app-is.md)

**`words.json`.** The list of every word an app's sources give `t()`, written by the build for administrators to
translate. [19](19-languages-and-formats.md)

**Zone.** A rule that limits where staff credentials work (a network, a host), refused with `zone`.
[05](05-permissions-and-people.md)

## Checked by

Nothing: a glossary is reviewed. Each entry points at the guide that cites its source.

## See also

[README.md](README.md) for the map of the handbook; `docs/app-contract.md`; `docs/api.md`.
