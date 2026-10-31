# Ferrum as one platform for the whole enterprise: plugins, Office, Mail

Plan, 30 October 2026. Nothing in it is built unless marked **today**, except Phase 1 (plugin platform
v2: extension points, add-ons and screens), which is in docs/plugins.md "Extension points" and "Screens",
and the parts of Office (§4) and Mail (§5) whose status lines say they are built.

[apps-ui.md](apps-ui.md) (30 October 2026) plans an empty core with apps that own their interfaces; it
replaces decision 3 below, §3.4 Tier 2 and part of §7 (its §21 lists what changes).

## 1. Short answers

**Does our webmail have spam filtering and corporate features such as groups?**
Yes (**today**, Comms 0.4.0, §5). Ferrum hosts mail on a Stalwart server beside the stack: one mailbox
per person in the workspace's domain, a webmail in the app, shared mailboxes and distribution lists, and
filing a mail onto any record. Spam filtering and its training are Stalwart's. Workspaces on Google
Workspace or Microsoft 365 keep connecting their mailboxes over SMTP/IMAP, as before. Comms also threads
mail onto records, sends templates and newsletters to email groups (with one-click unsubscribe), and
has team chat and calendars.

**Is the system plugin-based?**
Yes, at the core (**today**): every app is a package with a manifest, a runtime (native Rust, sandboxed
WebAssembly or metadata only), dependencies with version ranges, and settings. The 15 app repositories
(Drive, Comms, CRM, HR, Helpdesk, Commerce, …) and the ERP modules are built this way. WebAssembly plugins
add document hooks, HTTP endpoints, WebSockets, event handlers, schedules and storage. Since Phase 1
(**today**, §3), apps also offer extension points to others, have add-ons (sub-plugins) and bring their
own screens, sandboxed and checked in Chromium, Firefox and WebKit (spike S4).

**Do we reuse existing projects and libraries?**
Yes, at three levels:
* Reference: ERPNext/Frappe (cloned in `upstream/`, analysed in `docs/analysis-erpnext.md`) defines the
  business behaviour we reproduce.
* Libraries: Loco/Axum, sqlx, wasmtime, MiniJinja, CEL, OpenDAL, lettre, mail-parser, async-imap, argon2,
  jsonwebtoken, rust_xlsxwriter, qrcode; React, TanStack, Radix/shadcn, recharts, dnd-kit,
  react-grid-layout.
* Whole services: PostgreSQL, S3-compatible storage, Gotenberg (PDF rendering).

§2 turns this into a rule and applies it: Stalwart for mail, Collabora/ONLYOFFICE for office documents.

**Can DeckCraft, GridCraft, WordCraft and PdfCraft become our office plugin?**
Yes, as the add-on Browser editors under Drive (**today**, Phase 4a,
[browser-editors.md](browser-editors.md)). Office is built (**today**, §4) with Collabora Online or ONLYOFFICE through WOPI, inside
Drive.
* The four apps are open source (MIT/Apache-2.0) and run in the browser.
* There is no hosted web version to wrap, so Ferrum serves their official, unmodified web builds from
  an editor server on an origin of their own.
* Drive opens and saves files through the host bridge GridCraft, WordCraft and PdfCraft have (spike
  S3). DeckCraft follows once it has that bridge.
* Collabora stays the default.
* Their brand terms keep the ArtCraft name and logos out of the add-on's name, icon and listing.

**Collaboration like Google Workspace?**
Planned and partly built in [collaboration.md](collaboration.md):
* **Live co-editing of Office files:** the online editor (**today**).
* **Drive as a shared workspace (**today**):**
  * sharing with roles;
  * Shared with me, Recent, Starred, Trash and search;
  * comments with @mentions;
  * versions and activity;
  * who has a file open now;
  * requests for access.
* **Video meetings (**today**, C4):** calls in chat channels, meetings for calendar events and rooms
  started from Meet, on a LiveKit server the operator runs.
* **Next:** co-editing made visible (C2), documents co-edited natively (C3), calendar and contacts
  sync and guests (C5).

**Own dashboards, endpoints, custom screens, nested plugins?**
* Dashboards: without code, through package seeds of Studio spaces, dashboards and charts. No app
  does it yet; the first example proves it.
* Endpoints: **today** for WebAssembly plugins, and an app's own `/api/ext/<app>/…`.
* Custom screens and nesting: **today** (§3; ferrum-sdk `examples/board`). The first real add-on
  and extension point are Browser editors under Drive's `drive.editor`.

Naming: "workspace" already means a tenant in Ferrum, so the document suite is called **Office** below.

### Decisions to confirm
1. Office: Collabora by default through a WOPI host we write once; ONLYOFFICE as a second provider on the
   same host; the ArtCraft editors as an optional add-on (S3 passed; [browser-editors.md](browser-editors.md)).
2. Mail: Stalwart as a separate service that Ferrum provisions; our own webmail over JMAP.
3. Plugin screens: sandboxed frames for anyone's plugins; first-party screens stay in the web app.
4. Nesting: a `parent` field on top of `requires`; no new lifecycle.

## 2. Principles

### How any requirement fits

| Need | Layer |
|---|---|
| A field, form, list, report, dashboard, workflow, automation, print format | No code: metadata, Studio, automations, per tenant |
| Logic, or an integration with an outside API | WebAssembly plugin: hooks, endpoints, schedules, events |
| A screen of its own | Plugin screen (§3.4) |
| Another provider for an existing capability (payment gateway, document editor, e-invoice connector) | Sub-plugin on the owning app's extension point (§3.2, §3.3) |
| A product area (HR, Helpdesk, Office, Mail) | An app, possibly fronting a separate service (§3.6) |

### Rules

1. **Small kernel.** It keeps tenancy, metadata, permissions, audit, events, files, print, workflow and the
   API/MCP. Everything else is an app.
2. **Apps meet only at declared contracts**: extension points, events, the public API. No app reads
   another's tables or calls its code.
3. **Integrate before build.**

   | It is | So we | For example |
   |---|---|---|
   | Business data where our model is the product | build on metadata; ERPNext is the functional reference | ledger, stock, payroll |
   | A protocol-heavy, security-critical server | run a mature open-source server as a separate service; Ferrum provisions and fronts it | mail, office editing, PDF rendering |
   | A UI or file-format component | use a library | tables, charts, spreadsheet export |
   | A vendor's service | write a connector sub-plugin | payment gateways, IRP/GSP, identity providers |

   A default must be stable and widely used; alpha projects only as optional providers. A dependency's
   licence must be compatible with AGPL-3.0 (Ferrum's), or it runs as a separate service.
4. **One mechanism per concern**: one registry, one manifest, one lifecycle, one bridge for screens. A
   sub-plugin is an ordinary plugin.
5. **An extension point needs two real providers** (or one now and one scheduled). Until then, call the
   single implementation directly.
6. **Every phase ends usable**: tests, docs, deployment files and a browser check, never scaffolding.

## 3. Plugin platform v2

### 3.1 Today

| Capability | Where |
|---|---|
| App manifest: id, version, runtime, platform range, `requires`, packages, settings (secrets sealed), uninstall policy | `ferrum/crates/apps/src/manifest.rs` |
| Metadata packages: entities, overlays on other packages' entities, actions with parameters, print formats, numbering, seeds | `ferrum/crates/meta` |
| WebAssembly plugins: hooks; endpoints `/api/ext/{plugin}/…` and public `/api/public/{tenant}/{plugin}/…`; sockets `/ws/ext/…`; events; schedules; kv; declared permissions and outbound hosts; fuel and memory limits | `ferrum/crates/plugins`, `ferrum/server/src/controllers/plugins.rs` |
| Studio: reports, charts, number cards, dashboards, spaces | `ferrum/crates/studio`, `ferrum/web/src/app/studio` |
| Promised, not built: UI slots in a sandboxed frame | `ferrum/docs/plan/architecture.md` §7.2 |

### 3.2 Extension points and contributions

Two optional manifest blocks, plus `parent` (§3.3).

The owning app declares a point:

```yaml
# office/app.yaml
id: office
requires: { drive: ">=0.1" }
extension_points:
  office.editor:                      # always prefixed with the owner's id
    schema: schemas/editor.json       # JSON Schema each contribution must satisfy
    description: Opens and edits files of given types
```

A provider contributes to it:

```yaml
# office-collabora/app.yaml
id: office-collabora
parent: office
requires: { office: ">=0.1, <0.2" }
runtime: native
settings:
  - { key: url, label: Collabora server, kind: text, required: true }
contributes:
  office.editor:
    - id: collabora
      kind: wopi
      discovery: "{settings.url}/hosting/discovery"
      types: [docx, xlsx, pptx, odt, ods, odp]
```

Rules:
* A contribution may target a kernel point, or a point of an app in its `requires`. It is checked against
  the point's schema at install and upgrade; an invalid one fails the install, not a page later.
* A point's contract changes incompatibly only with a major version of its owner, so `requires` ranges are
  the compatibility check; points carry no versions of their own.
* Contributions are read from the installed manifests and cached per tenant; no new table.
* Apps read them with one call: `contributions("office.editor")` in Rust,
  `GET /api/meta/contributions/{point}` from the web app.
* The existing manifest sections (hooks, routes, events, schedules, sockets, print formats, seeds) already
  are kernel extension points and stay as they are. `contributes` is only for the new screen slots and the
  points apps declare.

The first app-declared points, each with at least two providers:

| Point | Owner | Providers |
|---|---|---|
| `drive.editor` | Drive (Office lives there) | Collabora and ONLYOFFICE, built in; GridCraft, WordCraft and PdfCraft from the Browser editors add-on ([browser-editors.md](browser-editors.md)); DeckCraft once it has a bridge |
| `commerce.gateway` | Commerce | Razorpay, Stripe, PayPal (exist; registered here when next touched) |
| `india.einvoice` | India | direct IRP connection (exists), GSP connectors |

### 3.3 Sub-plugins

A sub-plugin is an ordinary app with `parent:`, which must also be in `requires`:
* Installing it needs the parent (the existing `requires` check).
* Disabling or removing the parent is refused while children are installed; the message lists them and
  offers to remove them together.
* Upgrading the parent checks the children's version ranges first.
* The Apps page and the store show children under their parent ("Add-ons for Office"). The registry stays
  one flat list with a `parent` column.

Depth is unlimited because it is just `requires`; the UI shows two levels. A sub-plugin may declare points
of its own when the two-provider rule holds (for example GSP connectors under India).

### 3.4 Plugin screens

Three tiers; use the lowest that works.

**Tier 0, no code.** Forms, lists, reports and prints come from metadata (**today**). An app ships its own
space and dashboards by seeding Studio's `Space`, `Dashboard`, `DashboardChart` and number-card records in
its package. To do: a first example (Commerce) that proves it, and a page in the docs.

**Tier 1, sandboxed screens (new, for anyone).** A plugin ships static files (`ui/`: HTML, JS, CSS,
WebAssembly) and contributes slots:

```yaml
contributes:
  ui.page:   [{ id: board, title: Shipments, icon: truck, entry: ui/board.html }]
  ui.widget: [{ id: late, title: Late shipments, entry: ui/late.html, size: [4, 3] }]
  ui.panel:  [{ id: tracking, entity: DeliveryNote, title: Tracking, entry: ui/tracking.html }]
```

| Slot | Shows |
|---|---|
| `ui.page` | a page with a sidebar entry at `/app/x/{plugin}/{page}` |
| `ui.widget` | a card to place on any Studio dashboard or space |
| `ui.panel` | a tab on the forms of an entity, given the record |
| `ui.settings` | the app's own settings page |

Isolation: each screen runs in `<iframe sandbox="allow-scripts">` without `allow-same-origin`, so it has
an opaque origin and cannot touch the app's page, storage or session. Its files come from
`/ext-ui/{plugin}/{version}/`, only for installed versions, with a Content Security Policy that lets it
load its own files and nothing else: no other network access, no form posts, no navigation. It can only
talk to the host.

Bridge (`@ferrum/ui` in ferrum-sdk, a few hundred lines): on load the host hands the frame a
`MessageChannel` port and answers typed requests over it:
* `context()`: user, locale, theme (CSS variables, so plugin screens look native), the record for panels;
* `api(method, path, body)`: the host makes the call to `/api/v1/…`, `/api/meta/…` or the plugin's own
  `/api/ext/{plugin}/…` and names the plugin; the server limits it to the plugin's declared permissions
  and the user's rights (the rule WebAssembly plugins follow **today**);
* `files.open()`, `files.save(bytes)`: through Drive, with the user choosing;
* `toast`, `confirm`, `navigate`, `openRecord`, `resize`; events when the record or the theme changes.

A plugin keeps its state in its kv through `api` (an opaque origin has no localStorage).

**Tier 2, first-party screens (today, as a pattern).** Large first-party experiences (POS, Studio, chat;
next the webmail and the Office launcher) are React code in the web app, loaded lazily and shown only when
their app is installed. Ferrum loads no trusted JavaScript at runtime.

### 3.5 Endpoints

**Today**: `/api/ext/{plugin}/…` (signed in), `/api/public/{tenant}/{plugin}/…` (webhooks, public pages),
`/ws/ext/{tenant}/{plugin}/…`. New: an optional OpenAPI fragment in a plugin's manifest, merged into the
API docs and offered to assistants as MCP tools.

### 3.6 Apps that front their own server

Collabora, ONLYOFFICE and Stalwart run as separate services. Ferrum does not deploy containers: operators
switch them on with a Compose profile or Helm values in `ferrum/deploy/`. The sub-plugin keeps the URL and
credentials (sealed settings), reports health on the admin status page, and provisions through the
service's API, retried from the outbox.

### 3.7 Phase 1 work

1. Manifest: `parent`, `extension_points`, `contributes`; validation on install and upgrade; the
   per-tenant cache and the `contributions` call.
2. Parent and child lifecycle; grouping on the Apps page and in the store (`ferrum-store`).
3. Tier 1: the four slots, `/ext-ui` with its policy, the bridge, the server-side permission limit for
   bridged calls.
4. An example plugin in ferrum-sdk (page, widget, panel, endpoint); `ferrum/docs/plugins.md`.
5. Commerce: gateways registered under `commerce.gateway`; a seeded dashboard (the Tier 0 example).

Tests: manifest validation; install, upgrade and removal of a parent with children; a plugin without
write permission cannot write through the bridge, even for an administrator; the frame's isolation in
Chromium, WebKit and Firefox (Playwright).

## 4. Office

**Status, 30 October 2026: Office v1 is built in Drive** (CBFerrum/ferrum-app-drive#1,
CBFerrum/Ferrum-Frontend#5, `deploy/office.yml`). Since Drive 0.3.0 (ferrum-app-drive#3) documents
can be as large as the operator allows (`FERRUM_DRIVE_MAX_FILE_MB`, up to 1 GiB).
* Drive is its own WOPI host, and the web app has a Files page and an editor page.
* One editor server per installation, set by the operator.
* Collabora and ONLYOFFICE both speak WOPI, so they are one provider at the code level, not two.
  The second kind, browser editors over their host bridge, comes with the `drive.editor` point in
  Phase 4a (§4.3), as rule 5 of §2 asks.
* Checked end to end with Collabora Online 26.04: a new document, typing, and a version saved when
  the person left.

### 4.1 Shape

```
Drive (today: files, folders, versions, sharing with people and teams, links, trash, quotas, search;
│      Office: the WOPI host, a Files page and an editor page)
├── drive.editor          Phase 4a: the editors offered for file types
├── online editor         today: Collabora (default) or ONLYOFFICE over WOPI, set by the operator
└── drive-editors         Phase 4a: the add-on Browser editors (GridCraft, WordCraft, PdfCraft)
```

As built, Office lives in Drive and the WOPI editor is set by the operator, not a sub-plugin. Drive
creates blank documents, sheets and presentations, or ones from a template, and lists recent files.
Templates (Drive 0.11.0) come with Drive, as Word, Excel and PowerPoint files it writes itself
(meeting notes, a project brief, a report, a letter, a budget, an expense report, a task tracker, a
project update, a team meeting), and from the workspace: a Drive administrator saves any Office file
as one (`DriveTemplate`, which keeps its own copy of the file). People pick them in Drive's New menu
(Sheets and Slides offer their own); agents use `list_templates` and `create_from_template`. Docs'
live documents have their own gallery in Docs.

* Opening a file in Drive opens its editor in a frame:
  * for WOPI, the engine's page with a short-lived access token for that user and file;
  * for a browser editor, a relay on the editor server ([browser-editors.md](browser-editors.md)).
* The WOPI host (CheckFileInfo, GetFile, PutFile, Lock, Unlock, RefreshLock, GetLock, PutRelativeFile) is
  written once and serves Collabora and ONLYOFFICE alike. Microsoft 365 for the web speaks it too but needs
  Microsoft's partner programme: not planned.
* Each save is a Drive version; Drive's sharing decides who may open or edit; the engine provides
  co-editing; locks follow WOPI.
* Security: tokens are per user and file, short-lived and accepted only on the WOPI endpoints.
  Proof signatures are checked when the engine publishes its keys; spike S1 found the stock
  Collabora image publishes none, so deployments mount a proof key, and WOPI endpoints accept only
  the editor server's address ([spike-s1-collabora.md](spike-s1-collabora.md)).
* PDFs: viewing with the browser's own viewer; pdf.js (Apache-2.0) only if we add annotation; Gotenberg
  already converts to PDF; signing stays in the Sign app. Whether PdfCraft's Rust engine is worth using on
  the server (merge, split, fill) is part of spike S3.

### 4.2 The editors compared

| | Collabora Online | ONLYOFFICE Docs | ArtCraft web builds |
|---|---|---|---|
| Licence | MPL-2.0 | AGPL-3.0 | MIT / Apache-2.0 (names and logos excluded) |
| Maturity | mature (LibreOffice core) | mature | young (0.x; DeckCraft alpha) |
| Embedding | WOPI | WOPI and its own Docs API | a host bridge to a same-origin parent (not DeckCraft yet) |
| Co-editing | yes | yes | no |
| Runs | server (container) | server (container) | in the browser; a static editor server |
| To know | Nextcloud's office suite is built on it | dispute around the Euro-Office fork | canvas UI, little for screen readers; ArtCraft name and logos only inside the apps |

### 4.3 Browser editors (Phase 4a)

**Status, 30 October 2026: built.** Drive 0.4.0 with its add-on Browser editors, Ferrum's
`deploy/editors`, and the web app's editor page. Checked end to end in Chrome: a workbook opened from
Drive, edited and saved as a version. The editor server is tested in Chromium, Firefox and WebKit in
CI. Fidelity on 25 public files: [browser-editors-fidelity.md](browser-editors-fidelity.md). Still
to come: DeckCraft, once its host bridge exists (asked for in deckcraft#1563;
[upstream-requests.md](upstream-requests.md)). Spike S3
([spike-s3-artcraft.md](spike-s3-artcraft.md)) found go without a fork.
* An editor server the operator runs, on an origin of its own, serves the official, unmodified web
  builds of GridCraft, WordCraft and PdfCraft, and a small Ferrum relay.
* Drive declares `drive.editor`; the add-on Browser editors (`drive-editors`, a manifest only)
  contributes the three editors.
* Drive's editor page frames the relay, which frames the app and passes `open`, `save` and `dirty`
  between them. Each save is a Drive version.
* Editing is single-user: the 30-minute editor lock, now shared with WOPI sessions, keeps one session
  per file.
* DeckCraft, a Save button and read-only mode wait for changes proposed upstream, not a fork.

### 4.4 Not in Office

Our own editor or renderer; co-editing for browser-only editors; Microsoft 365 for the web; Univer (an
Apache-2.0 spreadsheet SDK: a candidate later for spreadsheet-like fields inside forms, not for files).

## 5. Mail

**Status, 30 October 2026: hosted mail v1 is built in Comms 0.4.0** (CBFerrum/ferrum-app-comms#1–#3,
CBFerrum/Ferrum#5, CBFerrum/Ferrum-Frontend#6, `deploy/mail.yml`).
* One mailbox per person in the workspace's mail domain, on a Stalwart server beside the stack, kept
  in step with the person's account.
* A webmail in the web app, over JMAP through Ferrum: safe message HTML, attachments, app passwords,
  and filing a mail onto a record.
* Shared mailboxes and distribution lists.
* Checked locally against Stalwart 1.0.1 through the HTTP API and the web app (sync, webmail, a reply
  from a shared mailbox reaching the sender, threaded). Spam training, mail through a relay and the
  rest of Phase 3's done-criteria rest on Stalwart, as spike S2 found; they are not in CI.

### 5.1 Shape

```
Comms (today: mail through any provider's SMTP/IMAP on records, templates, newsletters, chat, calendars)
└── comms-mail-server     new: mail hosted by Ferrum on Stalwart (a separate service), with webmail
```

Tenants on Google Workspace or Microsoft 365 keep using Comms as today; tenants who want Ferrum to host
their mail install the sub-plugin.

Spike S2 ([spike-s2-stalwart.md](spike-s2-stalwart.md)) checked Stalwart 1.0.1 and found go,
with three corrections to this section:
* administration is JMAP, not a REST API;
* multi-tenancy and SCIM need the Enterprise edition. Without them, workspaces share one instance
  kept apart by their domains: accounts carry `externalId = ferrum:<workspace>:<person>`, and
  ordinary users cannot list other accounts, because directory queries are off by default. Hard
  separation needs one instance per deployment, or Enterprise tenants;
* group accounts are the shared mailboxes.

### 5.2 What Stalwart does and what Ferrum adds

Stalwart (Rust; AGPL-3.0, with a paid Enterprise edition) is a complete mail server: SMTP, IMAP, JMAP,
POP3, Sieve, spam and phishing filtering, DKIM/SPF/DMARC/ARC, mailing lists, sign-in through OpenID
Connect.

Ferrum adds:
* **Provisioning, with Ferrum as the source of truth**: domains; a mailbox per user (deactivating the user
  disables it); aliases; distribution groups from teams or departments, or by hand; shared mailboxes
  (sales@, support@) with members and rights. Synced idempotently through Stalwart's management API from
  Ferrum's events.
* **Sign-in**: webmail uses the Ferrum session. Desktop and phone clients sign in through Ferrum with
  OpenID Connect (Ferrum adds discovery and userinfo to its existing OAuth server) or with app passwords;
  spike S2 picks one. Stalwart's OIDC directory maps groups from a claim, but mailing lists cannot come
  from claims, so those are always provisioned through the API.
* **Domain setup**: the records to add (MX, SPF, DKIM, DMARC, autoconfig), checked live.
* **Webmail** (Tier 2, over JMAP through a Ferrum proxy, so there is one origin and one session): folders
  and labels, conversations, search, compose with attachments from Drive, filing to records (reusing the
  Comms threading), signatures from organisation templates, vacation replies, shared mailboxes,
  delegation.
* **Spam**: a Junk folder; "spam" and "not spam" train Stalwart's classifier; allow and block lists per
  user and per tenant; a quarantine view for administrators.
* **Rules**: a rule editor (if from / to / subject / size, then move / label / forward / reply) that writes
  Sieve; organisation-wide rules for administrators.

### 5.3 Not in Mail

Our own SMTP/IMAP/JMAP server, spam engine or list manager; webmail over plain IMAP (JMAP only);
retention, legal hold and journaling until a customer asks (check Stalwart's support then).

### 5.4 Deliverability

Many clouds block outgoing port 25, and new addresses have no reputation. The deployment files let
operators relay outgoing mail through a provider (Amazon SES, Postmark, …) and document reverse DNS; the
domain page shows whether outgoing mail works.

## 6. Phases

| Phase | Delivers | Done when |
|---|---|---|
| 0. Spikes: timeboxed, throwaway code, a written go/no-go each | **S1** (2 days) WOPI host against Collabora's development edition: open, edit and save a DOCX from Drive; proof keys; locks; current terms for production use. **S2** (2 days) Stalwart: provision a domain, user, group and list through its API; list, read and send over JMAP; OIDC sign-in with a Ferrum token; junk training; how hosting many tenants on one server is licensed. **S3** (3 days) ArtCraft: build the web targets, add the bridge, round-trip ten real XLSX, DOCX and PPTX files, measure size and load time; PdfCraft's server engine. **S4** (1 day) a Tier 1 frame and the bridge under the planned policy in Chromium, WebKit and Firefox, including a WebAssembly bundle. | four notes in `ferrum/docs/plan/` |
| 1. Plugin platform v2 | §3.7 | an example plugin with a page, widget, panel and endpoint installs from the store under its parent; the permission tests pass |
| 2. Office | §4 with Collabora and ONLYOFFICE, templates, Compose profile, Helm values | a document, a sheet and a presentation are created, co-edited and versioned from Drive in the browser; WOPI tests; the deployment smoke test covers it |
| 3. Mail | §5 | a tenant adds its domain; users get mailboxes; mail flows both ways through a relay; spam lands in Junk and training works; a shared mailbox and a distribution list work; a mail is filed to a record |
| 4a. Browser editors | [browser-editors.md](browser-editors.md): the editor server, Drive's `drive.editor` point and bridge editing, the add-on, the web app, deployment files, a fidelity note, upstream pull requests | a sheet, a document and a PDF open from Drive, are edited and saved as versions in Chromium, Firefox and WebKit; a second user gets read-only; online and browser editor sessions exclude each other |
| Phase 1 remainder | Commerce: its gateways under `commerce.gateway` and a seeded dashboard (the Tier 0 example, §3.4); plugins' OpenAPI fragments in the API docs and as MCP tools (§3.5) | when Commerce is next changed; the OpenAPI part when asked for |
| 4. When asked for | DeckCraft in Browser editors (once its bridge is released); calendar and contacts sync over CalDAV/CardDAV; GSP connectors; Microsoft 365 and Google connectors; a marketplace for third-party plugins (signed OCI packages, architecture §7.5) | |

Order: 0, 1, 2, 3, then 4a (built 30 October 2026). Video meetings were built as collaboration C4
(collaboration.md).
* The platform came first because Office and Mail are its first users.
* Office came before Mail because it is smaller and proves the separate-service pattern.
* 4a is the platform's first real add-on and extension point.

## 7. Not building

* Container orchestration in Ferrum: operators deploy services with Compose or Helm.
* Runtime loading of trusted JavaScript (module federation): first-party screens ship in the web app.
* An extension point with one provider (no "mail engine" interface: Stalwart only).
* A rewrite of the working manifest sections into `contributes`.
* Our own mail server, spam engine, office editor or PDF engine.
* A database per plugin: plugins use metadata entities and kv on the shared PostgreSQL with row-level
  security.
* Marketplace billing before third parties publish plugins.
* Lifecycle code for nesting beyond `parent`.
* Production Office or Mail code before the spike notes.
* A fork of the ArtCraft apps, or their names and logos anywhere outside the apps themselves.
* An origin per plugin before a third-party plugin needs one (browser-editors.md §12).

## 8. Risks

| Risk | Answer |
|---|---|
| Hosting many tenants on one Stalwart needs its Enterprise edition (S2 confirmed: `x:Tenant` and SCIM are Enterprise) | the open-source edition keeps workspaces apart by domain, with directory queries off; Enterprise tenants where hard separation is required |
| The free builds of Collabora and ONLYOFFICE limit or discourage production use | S1 checks current terms; budget a subscription or build from source; both sit behind the same WOPI host, so switching is a setting |
| ONLYOFFICE's governance dispute | Collabora is the default |
| ArtCraft: young (DeckCraft alpha), canvas UI with little for screen readers, one vendor, brand terms | an optional add-on with Collabora the default; unmodified pinned builds; what is missing proposed upstream; no ArtCraft name or logo in Ferrum ([browser-editors.md](browser-editors.md) §13) |
| AGPL: changing Stalwart or ONLYOFFICE means publishing the changes | run them unchanged; Ferrum is AGPL already, so there is no licence conflict |
| Sandboxed frames behave differently in WebKit | S4 tested all three engines ([spike-s4-frames.md](spike-s4-frames.md)): WebKit refused a screen's fetches of its own files under `'self'`, fixed by naming the server's origins in the screens' policy; all checks pass in Chromium, Firefox and WebKit |
| Editor servers need a lot of memory | size them from a load test in Phase 2; scale them separately |
| Deliverability | relay option and live checks (§5.4) |
| "All in one" grows without end | one suite at a time, each with the done-criteria above |

## 9. Sources

* Stalwart: https://github.com/stalwartlabs/mail-server, https://get.stalw.art, OIDC directory
  https://stalw.art/docs/auth/backend/oidc/, mailing lists https://stalw.art/docs/ref/object/mailing-list/,
  lists and groups with external directories
  https://support.stalw.art/t/use-mailing-lists-instead-of-groups-with-external-directories/1692,
  https://github.com/stalwartlabs/stalwart/discussions/2626
* Collabora and ONLYOFFICE:
  https://blog.elest.io/collabora-online-vs-onlyoffice-which-self-hosted-office-suite-after-the-euro-office-fork/,
  https://contabo.com/blog/nextcloud-collabora-vs-onlyoffice
* Univer: https://github.com/dream-num/univer
* ArtCraft: https://getartcraft.com/apps/deckcraft, https://getartcraft.com/apps/gridcraft,
  https://getartcraft.com/apps/wordcraft, https://getartcraft.com/apps/pdfcraft,
  https://github.com/storytold/deckcraft, https://github.com/storytold/gridcraft,
  https://github.com/storytold/wordcraft, https://github.com/storytold/pdfcraft; brand terms
  https://github.com/storytold/gridcraft/blob/main/docs/brand/LICENSE-brand.txt
