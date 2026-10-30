# Ferrum as a suite of apps: an empty core, and apps that own their interfaces

Plan, 30 October 2026, third revision. It replaces the two drafts written earlier the same day
(§22 lists what changed) and two decisions of [platform.md](platform.md) (§21).

**Status, 30 October 2026.**
* **U0, spike: done in Chrome** ([spike-u0-apps-ui.md](spike-u0-apps-ui.md)); Firefox and WebKit
  still to run.
* **U1, shell platform: built.**
  * Kernel: `crates/kernel/src/shell.rs` and migration 021, for sessions per device, hosts,
    kinds of people and interface releases.
  * Manifest: the `ui` section and the `shell.*` points (`crates/apps/src/shell.rs`), and releases
    prepared from built files (`ui.rs`).
  * Server: `server/src/shell.rs` and `controllers/shell.rs`, for entry pages, `/_ui/`, `/_shell/`,
    `/api/shell`, `/open`, status pages and the self-removing worker.
  * Sessions: cookie sessions with Fetch-Metadata checks, and bound tokens for today's web app.
  * Runtime: the bar runtime (`server/shell/`), with the launcher, intents, companion windows, the
    shared live channel and version checks.
  * Not yet: `ferrum-app dev --ui`.
* **U2, frontend split: built.**
  * Ferrum-Frontend packages: `@ferrum/client` (the bootstrap, the shell API with fallbacks,
    requests with the browser's session) and `@ferrum/testing`. `@ferrum/kit`, `@ferrum/records`
    and `@ferrum/bridge` are entry points over today's modules, which move as apps leave the web
    app.
  * Apps: `apps/shell` (Home, the store, administration, the account with signed-in browsers, and
    records at `/data/e/{entity}/{id}`) and `apps/erp` (ERP at `/erp/`). They are built to
    `dist-apps/` with an index of digests; the server fetches them from the web app's image
    (`FERRUM_UI_SOURCE`).
  * Today's web app picks up the browser's session and sends a workspace on the shell to its new
    pages.
  * Server: the `shell_v2` setting, carried in the schema so every replica knows at once; the
    shell's pages; ERP as an app, installed wherever its packages are; records claimed by package
    (`shell.open` with `package`); launcher entries into today's web app for Comms, Drive and Docs.
  * Not yet: ERP's main script is 299 KB compressed, against a 200 KB budget; the packages are
    built from source in Ferrum-Frontend, not yet published to npm.
* **U3, empty core: step 1 built (the platform's packages).**
  * `core` keeps Company, Currency, ExchangeRate, Uom and (until the Projects app takes it) Project.
    New packages: `parties` (Customer, CustomerGroup, Supplier, SupplierGroup) and `products` (Item,
    ItemGroup, ItemUom, PriceList, ItemPrice). Account, CostCenter, FiscalYear, TaxTemplate, TaxRow
    and ItemTaxRate went to `accounting`, Warehouse to `inventory`, ItemSupplier to `buying`.
  * ERP-only fields became overlays of ERP's packages with `column: true`. They keep their names,
    columns, data, field order and place in the form: the entity's form may list them, and drops
    them where their package is missing. Examples: a company's default accounts, a customer's
    receivable account and credit limit, an item's stock, buying and accounting settings.
  * The full schema is identical before and after (`ferrum task deps_report schema:`, compared on
    every entity, field, column, form, role, menu and personal-data rule).
  * The catalog now builds every package with only its dependencies installed, and refuses one that
    reaches further. Its old base-only checks are gone. Two packages may not add the same field.
  * The domain's hooks run only where their package is installed. A test keeps every document hook
    scoped to a package.
  * ERP's manifest lists its five packages; India is its add-on. New workspaces get the core only.
  * The one-time `core-split` step moves existing workspaces onto the split (operations.md,
    Upgrades). `ferrum task upgrade_back step:core-split` undoes it before going back.
* **U3 step 2 built: contacts, and CRM on its own.**
  * `contacts` (Contact, Address, ContactEmail, ContactPhone) moved from CRM to the platform, with
    the hooks for primary emails and phones, address titles and primary addresses. Those hooks
    now cover whatever an address links to.
  * `parties` adds the customer and supplier links on contacts and addresses, and copies a primary
    address onto the customer or supplier. CRM adds the lead and prospect links and the address
    rule.
  * CRM 0.2.0 depends on core, contacts, parties and products. Its connector app `crm-erp`
    (package `crm_erp`, shipped with CRM) brings quotations' tax templates and taxes, "Create
    Sales Order", and sales orders' link to their quotation.
  * CRM's test runs a workspace with CRM alone: quotations without taxes, no ERP records or hooks,
    no sales-order action.
  * The `crm-erp` upgrade step gives every workspace with CRM and selling the connector. On the
    demo workspace, quotations kept their taxes and sales orders their quotation links.
  * A column field the form does not list goes into the section of the field it follows (CRM's
    links on contacts). The app API version is 0.3.2.
* **U3: Helpdesk on its own, suites and the first-run picker built.**
  * Helpdesk 0.2.0 depends on core, parties and products. Its `helpdesk-erp` connector brings
    tickets' sales invoices and maintenance contracts' sales orders; the `helpdesk-erp` upgrade
    step installs it where Helpdesk and selling are. Connector steps are one table.
  * Suites: a metadata app with only the apps it requires (category `suite`); Workspace, Business
    and Retail, listed where the server has their members. Store categories `suite` and
    `connector`.
  * The first-run picker: an administrator of a workspace with no apps gets suites and apps to
    install on Home (today's and the shell's). Checked in Chrome: a new workspace has only
    Administration; installing Business brings ERP, CRM, Helpdesk, Projects and their connectors.
* **U3: HR and payroll on their own built.**
  * HR 0.2.0: `hr` depends on core, `payroll` on `hr`. Payroll runs make and submit salary slips
    and post only where ERP is.
  * Its connector app `hr-erp` holds two packages: `hr_erp` (expense claims and advances moved
    whole, the company's employee accounts, journal entries' HR references) and `payroll_erp`
    (salary components' accounts, payroll cost centers, payroll entries' accounts and bank entry,
    the company's payroll accounts). The `hr-erp` step installs them where HR and accounting are.
  * The domain's company set-up creates warehouses only where inventory is.
* **U3: Project as a shared record (the §20.4 alternative), built.** Moving Project into the
  Projects app proved costly: ERP's ledger has a foreign key to it, and Finance's budget engine
  reads project columns across budgets, orders and journal lines in SQL. Every build or test
  without the Projects app would break, with budgets and journals with them.
  * Project left `core` for the hidden platform package `project_records`, which ERP's accounting
    depends on. ERP's documents, budgets and expense claims keep their project fields.
  * The Projects app 0.2.0 depends on core, parties and project_records. Its connector app
    `projects-erp` (package `projects_erp`) brings only Projects' invoicing: "Create Sales
    Invoice", service items, billing item and cost center, and the invoices' links.
  * The `project-records` step installs the package in a workspace that has projects without
    ERP. An entity's list columns, like its form, may name fields other packages add.
  * Commerce, POS, Field service, Assets and Bookings 0.1.1 require the ERP app, so installing one
    brings ERP whole.
  * Checked on new, empty workspaces: Commerce's plan installs ERP first; CRM installs alone (core,
    contacts, parties, products and crm, no ERP records).
  * Removing a suite offers its members that nothing else needs. Each installed app records the
    app whose installation brought it in (`tenant_app.brought_by`); an app installed for itself
    never counts. Checked on a new workspace: CRM installed alone, then Business, then removing
    Business offered its six other apps in a safe order and left CRM.
  * Comms 0.7.1 depends on `contacts`: every workspace with Comms has the shared address book
    (calendar and contacts apps read it), with or without CRM. Forms' and LMS' tests install what
    they use, now that core no longer has customers or ERP's roles.
  * U3 is complete.
* **U4b, CRM: its interface built.**
  * CRM 0.3.0 has its own interface at /crm/, in its repository's `ui/` (built with
    Ferrum-Frontend's packages, `dist` committed, embedded as `UI`). Its home shows the pipeline
    (open deals by sales stage, worth and weight), your open activities and new leads. Its records
    use the records frame, which `@ferrum/records` now exports for app repositories, with CRM's
    modules in the sidebar; the menu lists customers and items too.
  * Launcher entry, New Lead and New Deal in the Create menu, and /open of CRM's records, contacts
    and customers. Checked in Chrome on the demo workspace.
  * Not yet: making `shell_v2` the default, `/app/` only redirecting, and retiring the nginx image.
    Helpdesk, People, Projects, POS and Studio would be records-only until U5 gives them
    interfaces.

* **U4a, Comms and Workspace: built** (31 October 2026).
  * Comms (Mail, Chat, Calendar, Meet at `/mail/`, `/chat/`, `/calendar/`, `/meet/`), Drive (`/drive/`,
    with Sheets, Slides and PDF as entries of their own) and Docs (`/docs/`, the editor at
    `/docs/d/{file}`) have interfaces of their own in their repositories' `ui/`, embedded in their
    crates; Ferrum-Frontend holds none of their code, and today's web app sends their old addresses
    (bookmarks, links in mail, public links) to the new pages for every workspace.
  * Workspace is a family (Drive heads it with `ui.family`; Docs joins with `parent: drive`): one
    launcher group, one navigation (`FamilyNav`), separate releases.
  * Intents: `pick`, `preview` and `share` a file (Drive); `compose` an email, `schedule` an event,
    `share` a record to Chat (Comms). Mail attaches from Drive; Docs shares through Drive's dialog.
  * Side panels: Chat from the bar; Drive's file details as a contextual panel (Docs opens it beside a
    document). Meet calls open from Chat and Calendar in a companion window.
  * Uploads go through the bar runtime (IndexedDB, a tray, resumed by the next page of any app, one
    tab per upload by Web Locks); Drive stores an upload sent again once (`Idempotency-Key`, taken in
    the transaction that makes the file), and site requests that change something finish even when
    the browser leaves.
  * Public pages (`shell.public`): Drive's shared links (`/drive/s/…`) and Meet's guest page
    (`/meet/guest/…`), with no bar, no person, no referrer, no indexing.
  * Policies name operator settings (`env:FERRUM_OFFICE_ORIGIN`, `env:FERRUM_MEET_URL`), forms
    (`form`) and secure images (`https:`, Mail's remote images on request).
  * Checked in Chrome: sign-in, the family, an upload across a page load stored once, Sheets, a new
    document typed and saved, Drive's share intent and details panel in Docs, Chat in the panel kept
    across a page load, the schedule, pick and share intents, a companion window across a page load,
    a public link signed out.
  * Design pass: the bar draws Lucide icons (each launcher entry's `icon:` on its colour, and the
    bar's own buttons), follows the person's light or dark theme (the runtime puts the appearance
    stored on the device on `<html data-theme>` before the app draws, so the kit's `dark:` styles
    work in every app, and apps of their own design get the same signal), stays the page's width
    above a side panel, and groups the launcher: apps on their own first, then each family under its
    name. The shell's dialogs stay centred whatever reset an app brings. The kit has `AppIcon` (an
    app's icon as a glyph or a tile) and draws `FamilyNav` with it; Drive's list and picker and Docs'
    lists use the kit's components and file-kind icons, and live documents show no `.fdoc`. Drive
    0.8.1 and Docs 0.2.1 carry it.
  * Not yet: Firefox and WebKit runs (their Playwright builds are not on this machine); the Assistant
    panel (there is no assistant to put in it yet); Picture-in-Picture from the call window.

Contents: 1 where we are · 2 goals and targets · 3 what others do · 4 rules · 5 decisions ·
6 the person's view · 7 addresses and deployment shapes · 8 the core shell · 9 people, sessions,
identity · 10 the app contract · 11 apps inside and alongside other apps · 12 platform services ·
13 the empty core: splitting the data model · 14 moving from today's frontend · 15 scale ·
16 edge cases · 17 phases · 18 risks · 19 not building · 20 questions for the owner ·
21 what this replaces · 22 changes from the earlier drafts · 23 sources.

## 1. Where we are

Checked in the code on 30 October 2026:

| Area | Today | Why it is in the way |
|---|---|---|
| Web app | One SPA (Ferrum-Frontend, `web/`) at `/app/` | every app's pages ship in one frontend release |
| Navigation | `Shell.tsx` builds one sidebar from every metadata module of every installed package, plus hard-coded Mail, Drive, Studio, Apps, Admin and Customize links | a sales rep, an accountant and someone who only wants mail all get the same admin console |
| App pages | Mail, Chat, Calendar, Meet, Drive, Docs, the editors and POS are routes hard-coded in `web/src/main.tsx` | a new app needs a frontend change; an app's UI and its backend live in different repositories |
| Third-party UI | sandboxed screens (`ui.page`, `ui.widget`, `ui.panel`, `ui.settings`) at `/x/{app}/{page}`: opaque origin, MessagePort bridge | right for extending a page, not for a whole app |
| Sign-in | a JWT bearer token in `sessionStorage` (`lib/api.ts`); the workspace is typed on the login form | every tab signs in on its own; no host leads to a workspace |
| Revocation | `app_user.token_version` signs out every device at once | one lost phone cannot be signed out alone |
| Live updates | `/ws/live`, one WebSocket per page (`lib/live.ts`) | ten tabs, ten sockets |
| Data model | `core` holds Account, CostCenter, FiscalYear, TaxTemplate, Warehouse, Customer, Supplier, Item, PriceList and Project; CRM owns Contact and Address; CRM depends on `selling`, which depends on `accounting` and `inventory`; ERP vouchers and HR expenses link to Project | CRM cannot be installed without half of the ERP; Comms' CardDAV asks whether CRM's Contact exists |
| Links | `lib/links.ts` sends notifications to `/e/{entity}/{id}`, Drive items to Drive | links know which app shows what |
| Apps | 21 native apps compiled in (cargo features), WebAssembly and metadata apps from the store; `app.yaml` with `contributes`, `extension_points`, `parent`, `requires` | the right base: interfaces become more contributions |
| User files | `/files/{id}` is served inline with `Content-Security-Policy: sandbox` and `nosniff` | already safe on the app's origin; must stay so |

What the owner asked for (30 October 2026): the core is an empty shell; a workspace installs ERP,
CRM, Ecommerce, Workspace (Docs, Sheets, Slides, PDF), Mail and so on as it needs them; every app may
own its complete interface, and some live inside their parent's interface, as in Zoho; planned end to
end, practical, future-proof, covering today's and tomorrow's edge cases.

## 2. Goals and targets

1. **Empty by default.** A new workspace has only the shell. It installs apps, or suites of them.
2. **Every app chooses its interface**: its own, a place inside its parent's, generic record screens,
   or none (connectors, localisations).
3. **No platform release for an app.** Install, upgrade, disable and uninstall without a platform
   release or a frontend deployment; open tabs survive all four.
4. **Alone and together.** Apps work alone. When several are installed they cooperate without
   importing each other's code.
5. **One code base, every deployment**: one self-hosted server, a cluster, a multi-region cloud with
   custom domains, an air-gapped install.
6. **One app UI, every host**: a browser tab, an installed web app, a dialog or panel inside another
   app, a native mobile shell, a test harness, without changing the app.
7. **One product to the person**: one look, one bar, one search, one inbox, one assistant, consistent
   keys.
8. **Bounded failure.** A broken or hostile app cannot take over the workspace, and a first-party bug
   has a known blast radius.

Not goals: composing several apps' code into one page at run time; mandating a UI framework; keeping
copies of shared records per app (Zoho syncs CRM contacts into Books; we keep one record).

Targets the design is checked against (§15 says how each is met):

| Dimension | Target |
|---|---|
| Workspaces per cluster | 100 000 |
| People per workspace | 50 000 (largest), median 20 |
| Concurrent people per cluster | 200 000 |
| Apps in a store / installed per workspace | 1 000 / 60 |
| Launcher entries per person | 40 (Spaces, pins and search beyond that) |
| First load of an app, cold cache, 4G | interactive under 3 s; warm cache under 1 s |
| Switching apps, warm | under 1 s; near instant where prerendering exists |
| App or platform upgrade | no downtime; open tabs keep working or reload once |
| Regions | several; a workspace lives in one (data residency) |

## 3. What others do

| Product | How it is built | We take | We avoid |
|---|---|---|---|
| **Zoho One** | About 50 separate apps (CRM, Mail, Books, Desk…), each its own web app on its own host, one account. The November 2025 redesign adds Spaces on a top toolbar, Boards that gather work from several apps, one admin and integration panel, a Unified Portal and an assistant across apps; Zoho calls making 50 apps "look like one application" ongoing work. An account lives in one data centre (accounts.zoho.com, .eu, .in…); signing in elsewhere redirects to that one and back. | one identity, launcher, Spaces, admin, portal and assistant over separate apps; region-aware sign-in by redirect | per-app copies of customers kept in sync; separately built UIs that drift |
| **Google Workspace** | Separate apps on separate hosts. One bar served into every app. The account is chosen per URL (`/u/0/`). The Drive Picker is Drive's own UI in a frame any app opens. A side panel (Calendar, Keep, Tasks) in every app. Add-ons are cards defined once and rendered by every host, desktop and mobile. | a bar the platform serves; pickers that are another app's UI; side panels; cards | everything cross-origin |
| **Microsoft 365** | Is moving its apps to one domain (`cloud.microsoft`) to cut sign-ins, redirects and allow-lists. TeamsJS runs the same app in Teams, Outlook and the Microsoft 365 app and exposes each feature as a capability with `isSupported()`. | few hosts; one capability API for every host | hundreds of domains |
| **SAP Fiori launchpad** | The launchpad is the shell. Apps navigate by intent (`#SalesOrder-display`), resolved at run time to the app the person's role maps to it. The URL has a shell part and an app part. | intents resolved by role; the shell owns the first path segment, the app the rest | — |
| **Frappe** (our functional reference) | Desk is a generic UI. CRM, Helpdesk, Insights and others ship their own SPAs at `/crm`, `/helpdesk` on `frappe-ui` and register on the apps screen (`add_to_apps_screen`); an app is hidden when its permission check fails, its setup is unfinished or it is a companion of another app; a default app per person or site. | apps screen with permission check; default app; companions; generic records UI as fallback | two UIs (Desk and the app) for the same work |
| **Salesforce** | Lightning apps are navigation sets over one record UI. Softphones live in the console's utility bar, which persists because the console is one SPA. | navigation-only apps over the records UI | — |
| **Shopify** | Third-party apps run in frames in the admin and authenticate with short-lived session tokens. App Bridge is a script from Shopify's CDN, always current. Polaris web components have channel URLs (`polaris-1.js` follows major 1) and pinned ones (`polaris-1.0.js`). UI extensions run in a worker and render Shopify's components (remote-dom). | a platform script that stays current within a major; channel and pinned URLs; short app tokens; remote rendering for small extensions, later | — |
| **HubSpot, Atlassian Forge** | HubSpot UI extensions: React in a sandboxed worker, rendered into HubSpot's components through remote-ui. Forge Custom UI: the app's static files hosted by Atlassian, shown in a frame, talking through `@forge/bridge`. | the platform hosts app bundles; a bridge | — |
| **Next.js multi-zones, Vercel microfrontends** | Each zone owns paths on one domain; moving between zones is a full page load; client-side links into another zone break. | path ownership; other apps' paths are external links | — |
| **Nextcloud** | The server is a shell; apps bring their own bundles on a shared component library. | apps bring bundles; one component library | apps sharing one page and its globals |
| **Odoo** | One web client; apps add menus. | — | what Ferrum does today |
| **Vercel skew protection** | Old deployments' files are kept and each client stays on the version it loaded. | immutable, versioned files | — |
| **QuickBooks Online Accountant** | One login, many client companies; "switch company", "back to practice". | account chooser, later a partner view | — |

Micro-frontend practice says the same in general terms: a **vertical split** (each app a whole SPA
owning its routes, moving between apps is a page load) gives independent releases and the fewest run
time surprises; its cost is duplicated common UI, answered by a shared header and one design system.
Runtime composition (module federation) trades build-time integration for run-time coupling and is
the riskiest choice.

Web platform facts that shape the design:

| Fact | Consequence here |
|---|---|
| A Document Picture-in-Picture window closes when its opener navigates to another document, even on the same origin | a call cannot float over a page that is about to load another app; calls get their own window (§12.6) |
| Pages served `Cache-Control: no-store` are mostly kept out of the back/forward cache (Chrome allows some since 2025, for 3 minutes; other engines do not); `unload` handlers and open sockets also keep pages out | entry pages are `private, no-cache`; no `unload`; sockets live in a worker |
| SharedWorker exists in every desktop engine; Chrome for Android has it only since version 148 (2026) | one socket per browser where possible, one per tab otherwise |
| Cross-document view transitions: Chromium 126+, Safari 18.2+, Firefox not confirmed | app switches animate where supported, plain loads elsewhere |
| Speculation Rules prerendering: Chromium only | the launcher prerenders where it can, prefetches elsewhere |
| Fetch Metadata (`Sec-Fetch-Site`) is sent by every current browser; Go 1.25 replaced CSRF tokens with it plus an Origin fallback, and the `tower-csrf` crate does the same for Axum | our CSRF defence, refusing `same-site`, because sibling workspace hosts are same-site |
| Device Bound Session Credentials: generally available in Chrome on Windows since May 2026; the server needs registration and refresh endpoints | sessions are designed so a device-bound cookie can be added later |
| Passkeys across domains (Related Origin Requests) allow at most five registrable labels | thousands of custom domains cannot share passkeys; passkey sign-in happens on one accounts host |
| `Clear-Site-Data: "storage"` wipes every app's storage on the origin, offline queues included | sign-out never sends it blindly |
| Import maps with `integrity` (Chrome 127, Safari 18); compression dictionaries (Chrome and Edge 130+) | later ways to share React between apps and to ship upgrades as deltas |
| The web app manifest `id` lets one origin offer several installable apps; nested scopes behave differently between browsers | a suite app plus optional per-app installs, tested in U0 |

## 4. Rules

1. **Contracts, not shared code.** Apps meet through versioned contracts: the HTTP API, the
   bootstrap, the shell API, intents, slots, cards, events. Never by importing each other's modules
   at run time.
2. **Apps never assume where or how they run.** Origin, base path, API base, sign-in method and the
   kind of host (tab, frame, native shell) come from the bootstrap and the shell API. Path routes,
   subdomains, isolated origins, a frame host or a mobile shell are then settings, not rewrites.
3. **Capabilities, not version checks.** An app asks `shell.supports("intent")`; a missing
   capability has a defined fallback.
4. **Additive, versioned contracts.** The major version is in the URL (`/_shell/v1/`, `/api/v1`);
   within a major only additions; a deprecation lives two platform minors, with warnings in the
   browser console and on the admin status page.
5. **Hiding is not securing.** Launchers, routes and slots are convenience; the API checks every
   call, as today.
6. **One mechanism per concern** (platform.md rule 4): interfaces, launcher entries, intents, panels,
   portal pages and cards are `contributes` entries; suites and connectors are ordinary apps.
7. **Graceful absence.** Any app, connector, intent handler or service may be missing, disabled,
   failing or older; every consumer has a defined fallback (§16).
8. **One host for UI and API.** The browser talks to one host: no CORS, first-party cookies only
   (no third-party cookie rules, no Safari caps on cookies from other hosts).
9. **User content never runs as the workspace.** Uploaded files are served with a sandbox policy or
   as downloads (today's `/files` already does); in the cloud from a separate content domain; no
   workspace can run scripts on a host next to another workspace's.
10. **Immutable artefacts, movable pointers.** UI files never change once published; what a workspace
    runs is a pointer: the installed release, a pin, a canary.
11. **State that must outlive a page lives on the server or in a worker** (calls, uploads, assistant
    threads, drafts), never only in a page.
12. **Every phase ends usable** (platform.md rule 6); today's interface keeps working until the new
    one has replaced every page.
13. **One look by default, any look allowed.** Ferrum's own apps share one design system (the theme's
    tokens and `@ferrum/kit`, shadcn components), so the suite looks made by one hand; the shell owns
    only the bar. Any app may bring its own design and framework: the contract is files, the
    bootstrap, the shell API and the HTTP API, and an app's styles come after the theme's and win.

## 5. Decisions

**D1. Vertical split by path.** Each app's UI is a whole SPA owning its routes (`/crm/…`); moving
between apps is a document navigation; inside its routes the app does as it likes.

| Option | For | Against | Verdict |
|---|---|---|---|
| One SPA (today) | instant switching | every app in one release; one bug breaks all; the owner's complaint | no |
| Run-time composition (module federation, single-spa, import maps of app modules) | one page, shared React | version coupling between separately released apps at run time; one app's global CSS or JavaScript breaks another; hardest to debug | no |
| Frame host: a shell document with apps in frames (Teams; Shopify for third parties) | persistent bar and calls; instant switching between apps kept alive | history and deep links across frames; delegating camera, clipboard and WebAuthn; printing; memory; focus; every app tested twice | not by default; can be added later without app changes (rules 2–3, D5) |
| **Vertical split** (Google, Zoho's apps, Frappe's SPAs, Next.js zones) | independent releases; each app owns its document (CSP, worker, manifest, print, full screen); simplest | a page load between apps; nothing in a page survives a switch | **yes**, with the answers below |

The answers: the bootstrap is inline, so nothing waits for a request before the first render; UI files
are immutable and cached; the launcher prefetches or prerenders; cross-document view transitions
smooth the switch; the back/forward cache restores the previous app; the bar runtime runs on every page;
and work that must survive a switch moves out of pages (D12).

**D2. The core is a shell, not a suite.** Sign-in, Home, store, administration, account, bar, search,
notifications, assistant, generic data views, portal frame, first run (§8). ERP is an app.

**D3. The workspace comes from the host name** (§7): `acme.ferrum.cloud`, `erp.acme.com`, or the one
workspace of a self-hosted server. Path-per-workspace hosting (Google's `/u/0/`) is not offered: rule 2
keeps it possible, and shape A′ covers today's many-workspaces-on-one-host setups.

**D4. The platform puts the shell into every app page.** The server answers each app's entry page,
adding the bootstrap, the workspace's theme, the bar runtime and `<ferrum-bar>` to the app's own
`index.html`. Apps neither bundle nor include them. `/_shell/v1/` always serves the newest v1, as
Shopify's App Bridge does, so an app built two years ago still gets today's bar, theme and branding.
Considered and dropped: apps including `<ferrum-bar>` themselves (an app forgets, or pins an old one);
the bar as an npm package (every app drifts).

**D5. One shell API for every host.** `globalThis.ferrum.shell`, installed by the bar runtime (page),
a frame adapter (dialog, slot, panel, a future frame host, a foreign suite) or a native adapter
(mobile or desktop shell), with `supports()` per capability (§10.3). `@ferrum/client` wraps it and
falls back when it is missing. This is TeamsJS's model, and it is what makes rule 2 practical.

**D6. Cookie sessions per host**, revocable per device, ready for device binding; CSRF by Fetch
Metadata; bearer tokens unchanged for API, MCP, mobile and CLI; short-lived app tokens for isolated
UIs (§9, §12.3). Sign-in on the workspace's host (A, A′) or on a central accounts host (B, C).

**D7. One kit, pinned by each app; the look comes from the platform at run time.** Design tokens,
theme, branding and the bar arrive at run time; React components come from `@ferrum/kit` at the
version the app built with. Apps rebuild for new components, not for new colours.

**D8. Five ways for one app to appear in another, none sharing code at run time** (§11.1):
navigation, data contributions the host renders, frames, intent dialogs, and later remote rendering.
Each installable app owns its UI; a family (Drive and Docs) shares navigation, not a bundle (§11.5).

**D9. Trust decides where UI runs.** First-party UIs, UIs of store-verified publishers and UIs an
operator installs on their own cluster run on the workspace's host with the person's session.
Every other app's UI runs isolated (a sandboxed frame, or an origin of its own in shape D) with app
tokens limited to its declared permissions.

**D10. Apps work alone; couplings are connectors and intents.** Hard couplings (a won deal becomes a
sales order) live in connector apps that require both sides; soft ones ("email this customer") are
intents that appear when a handler is installed. Code never asks "is X installed"; it asks for a
capability, a handler or a contribution.

**D11. Content-addressed UI releases, per-workspace pointers** (§12.1). Any replica or CDN serves any
version; open tabs survive upgrades; a workspace can be pinned to, or rolled back to, a UI release;
canaries are a pointer.

**D12. Work that must survive navigation does not live in a page** (§12.6): calls and softphones in
a companion window, uploads in the bar runtime, long jobs on the server, assistant threads and drafts
on the server.

**D13. One data model.** Shared records live once, in small hidden packages; ERP-specific fields reach
them as overlays; fields that link across apps live in connectors (§13).

**D14. The shell is itself an app release.** Home, store, admin, account and data are the "shell"
app (route `/`): it upgrades, pins and rolls back like any app, and a white-label partner could ship
another shell later.

## 6. The person's view

```
https://acme.ferrum.cloud/                  Home: launcher, Spaces, My work, widgets        (shell)
                        /store/             install, update, remove apps and suites         (shell)
                        /admin/             people, teams, app access, domains, security…   (shell)
                        /account/           profile, security, devices, notifications       (shell)
                        /data/{entity}/…    generic list, form and report of any entity     (shell)
                        /open/{entity}/{id} goes to the app that shows that record          (shell)
                        /portal/            customers' and suppliers' portal                (shell + apps)
                        /erp/ /crm/ /helpdesk/ /people/ /projects/ /pos/ /studio/ …        (apps)
                        /mail/ /chat/ /meet/ /calendar/        (Comms: one UI, four routes)
                        /drive/ /docs/                         (the Workspace family)
                        /_shell/… /_ui/…    platform files: bar runtime, theme, app files
https://accounts.ferrum.cloud/              sign-in, account chooser, workspaces            (shapes B, C)
https://shop.acme.com/                      an app's public site on a host of its own (§7.3)
```

* Every page has the same bar (§12.2). Its launcher shows only apps the person may use, grouped in
  Spaces ("Sales" = CRM, Quotes, Helpdesk; set by admins, adjusted by each person), pinned ones first,
  then the most used (counted on the server from entry pages; no tracking in the browser).
* Inside an app, the app decides everything below the bar: navigation, home, screens, shortcuts.
* Add-ons appear inside their parent: Browser editors in Drive's "Open with", e-invoicing inside ERP,
  Sign's tab on CRM deals.
* One app only: people land in it. Several: Home, or the app last used (a preference); admins can set
  a default app per team.
* Links in notifications, emails, chat and search go through `/open` (§11.2); old `/app/…` links
  redirect, so no bookmark breaks.
* The suite installs as one web app ("Ferrum"), and each app optionally as its own ("Ferrum Mail").
* Several workspaces in one browser sit side by side, each on its own host; the account menu switches.
* Phones get the same apps, responsive; a native shell can host the same UIs later (§10.3).

## 7. Addresses and deployment shapes

### 7.1 Shapes

The same code runs in five shapes; an operator picks with settings.

| Shape | Addresses | Workspace from | Sign-in | For |
|---|---|---|---|---|
| **A** one workspace | `erp.acme.com/{route}/` | the server's default workspace | on that host | self-hosting (the default) |
| **A′** shared host | `erp.example.com/{route}/` | chosen at sign-in and kept in the session | on that host; one workspace per browser at a time | today's behaviour, development |
| **B** subdomains | `{workspace}.{base}/{route}/` | the subdomain | `accounts.{base}`, then a session per host | the cloud, large self-hosters |
| **C** custom domains | `erp.customer.com/{route}/`, as well as B | a verified domain | as B; the custom host gets its own session | the cloud |
| **D** isolated app origins | `{app}--{workspace}.{apps-base}/` (one wildcard certificate covers all) | the host | OAuth code + PKCE against the workspace; app tokens | third-party full UIs; optional for first-party ones |

Changing shape later (A to B, adding a domain) needs no app change (rule 2); old addresses redirect to
the new primary host.

### 7.2 Hosts

* **Host map** in the kernel: `tenant_host` (host, workspace, `target`, kind, primary, verify token,
  verified, released). `target` is `shell` (shell and apps), `portal` (portal pages only, such as
  `support.acme.com`) or `site:{app}` (an app's public site at the root, such as `shop.acme.com` for
  Commerce's storefront). Cached on every replica, invalidated by NOTIFY.
* **One primary host** per workspace and target. Links, emails and redirects use it; other hosts of
  the same target redirect to it, except while a move is in progress.
* **Unknown hosts** are refused before any application code runs: no Host-header tricks, and no DNS
  left pointing at us can show someone else's workspace.
* **Renamed or deleted workspaces**: their hosts redirect for 90 days, then stay quarantined for a year
  before anyone may claim them, so old links, emails, bookmarks and data stored in browsers never reach
  a new owner. As a second guard, `@ferrum/client` remembers the workspace id per origin in the browser
  and wipes that origin's storage if the host ever answers for another workspace.
* **Custom domains**: the admin adds a domain; Ferrum shows a CNAME and a TXT record and checks them.
  TLS is issued on demand by the edge (Caddy's on-demand TLS with an "ask" endpoint answered by Ferrum,
  or cert-manager with DNS-01 in Kubernetes), certificates in shared storage. Checked daily; after 7
  failed checks the domain is unverified and renewals stop; the subdomain keeps working. `deploy/`
  gets both recipes.
* **Fixed hosts for outside callers.** Identity providers (SSO), OAuth callbacks of mail and calendar
  connectors, payment and other webhooks use the server's canonical address (A, A′) or
  `accounts.{base}` and `hooks.{base}` (B, C), with the workspace in signed state or in the path. Adding
  a custom domain never means re-registering a callback with Microsoft, Google or a payment provider.
* **Public suffix.** In the cloud, workspace hosts live under a domain of their own that is listed in
  the Public Suffix List (as `github.io` is), and accounts and the website under another. Each workspace
  host is then its own site for cookies and SameSite. Until it is listed, CSRF checks refuse `same-site`
  requests (§12.3).
* **Regions**: the accounts directory knows each workspace's region and redirects to it; UI files are
  global (CDN); records never leave the region.
* **Development**: `{workspace}.localhost` reaches the loopback in Chromium and Firefox; with Safari,
  use shape A or A′.

### 7.3 Public sites

An app may own the root of a `site:{app}` host and serve public pages it renders on the server, so
search engines can read them: storefront, help centre, careers, booking pages, web forms. No bar, no
shell, a portal session at most. Today's `/api/site/{tenant}/{package}/…` and
`/api/public/{tenant}/{plugin}/…` paths keep working and redirect when the workspace has such a host.
Embedding a public widget in someone else's website is a site page with an explicit `frame-ancestors`
list set by the admin.

## 8. The core shell

| Part | What | Exists today as |
|---|---|---|
| Sign-in | password, 2FA, SSO, app passwords, the OAuth/OIDC provider; passkeys and the account chooser come with U6 | kernel `auth`, `twofactor`, `sso`, `oauth`; `Login.tsx` |
| Home | launcher with Spaces; "My work" (assignments, approvals, mentions, follow-ups from every app through `shell.work`); widgets; getting started | `Home.tsx` (module cards go), `ui.widget` |
| Store | browse, install with permission review, updates, add-ons under parents, suites, connectors offered when both sides are present, the first-run picker | `Apps.tsx`, `appmgr.rs`, ferrum-store |
| Admin | people, teams, roles, **app access**, Spaces, domains, sign-in policies, SSO and directory sync, sessions, audit, integrations (API keys, webhooks, connectors), data export, branding, app status (versions, errors, CSP reports), billing later | `Admin.tsx` |
| Account | profile, password, 2FA, devices and sessions, app passwords, notification preferences, language, theme, calendar and contacts setup | the user menu in `Shell.tsx` |
| Bar runtime | bar, launcher, search, Create, notifications, side panels, assistant, account menu, companion windows, uploads, live channel, version checks (§12.2) | the top bar in `Shell.tsx` |
| Data | generic list, form, report, import and export for any entity the person may read: the admins' tool and the fallback UI | `ListView`, `FormView`, `ReportView` |
| Portal frame | sign-in and layout for portal people; the pages come from apps | — |
| Status pages | not installed, no access, suspended, upgrading, sign-in needed, failed to start | — |

Studio (reports, dashboards, Spaces' pages, form designer, Customize) becomes the **Studio** app,
installed for admins by default, since Data's "Customize" opens it.

Not in the core: any business module, Mail, Drive, Studio's builders, POS.

## 9. People, sessions and identity

### 9.1 Kinds of people

The kernel gets one field, `app_user.kind`:

| Kind | Who | Sees |
|---|---|---|
| member | employees | the shell and the apps they have access to |
| guest | outside collaborators invited to a file, channel or meeting | only what was shared, through the owning app's guest pages (Drive and Meet do this today with links) |
| portal | customers, suppliers, students, applicants | the workspace's portal: pages apps contribute (`shell.portal`: Helpdesk "My tickets", ERP "My invoices", LMS "My courses") under its branding |
| service | integrations | no UI |

Portal people sign in with emailed codes or SSO, never see the bar or the launcher, and are counted
apart for billing. A membership has one kind; an employee who is also a customer uses their member
account, and apps may link that member to the customer's contact.

### 9.2 Sessions

* A **browser session** is a row in `user_session` (device, created, last seen, expires, user agent,
  IP, `actor`, `step_up_at`, later `device_id`) named by an HttpOnly, Secure, SameSite=Lax `__Host-`
  cookie. Last-seen is written at most every five minutes; replicas cache "still valid" for 20 seconds.
* People and admins see devices and sign them out one by one; `token_version` still signs out all.
* **Policies per workspace**: idle timeout, absolute lifetime, step-up for sensitive actions,
  allowed sign-in methods, IP allow lists.
* The token is **rotated** at sign-in, step-up and the start or end of impersonation.
* **Expiry mid-work**: a 401 opens sign-in in a dialog on the same page; the failed request is retried
  afterwards, so an open form loses nothing.
* **Step-up**: sensitive endpoints (security settings, exports, deleting a workspace) require a
  `step_up_at` under ten minutes old; the kit asks for the password, a passkey or 2FA and retries.
* **Impersonation and support access**: `actor` records who is really acting. The bar shows a banner
  in every app; the audit records both people; it is time-limited and needs a permission (and, for
  the vendor's support staff, the workspace admin's consent).
* **Device binding (later)**: with Device Bound Session Credentials, sign-in registers a device key
  and the cookie becomes short-lived, renewed only by proving that key; browsers without it keep
  today's cookie.
* **Shared devices (later)**: a POS terminal or warehouse scanner holds a device session; people switch
  on it with a PIN; offline queues stay with the device and sync as the person who recorded each entry.

### 9.3 One person, many workspaces

Today, a person in two workspaces is two unrelated `app_user` rows with two passwords. Shapes B and C
add a cluster-wide `identity` (email, password, 2FA, passkeys, home region) that each membership
(`app_user`) points to; shape A keeps credentials per workspace.

* Linking: at the first central sign-in the identity takes the credentials just used; other memberships
  with the same email join after the person proves each once (that workspace's password or an emailed
  code), never by email alone. New invitations join when accepted while signed in.
* An identity lives in one region. A workspace in another region trusts it through OIDC, as Zoho's
  secondary accounts trust the primary data centre; the workspace keeps its own copy of the member's
  name and email for residency.
* The account chooser lists the person's workspaces; later a partner view serves firms that work for
  many clients (accountants, implementers).
* Inside one workspace, the bootstrap carries the person's default company; apps that support several
  companies (ERP) let the person switch.

## 10. The app contract

### 10.1 Manifest

Interfaces are contributions to kernel points, plus a `ui` section:

```yaml
# ferrum-app-comms/app.yaml: only what interfaces add
id: comms
platform: ">=0.4, <0.5"
ui:
  dist: ui/dist                   # built files; part of the signed release digest
  shell: ">=1.2"                  # the shell API it needs
  csp:                            # extra origins for this UI's policy, reviewed by the store
    connect: ["setting:meet_url"] # an operator setting resolved at run time (LiveKit's address)
contributes:
  shell.app:                      # launcher entries; each owns a route, all served by this app's UI
    - { id: mail, route: mail, title: Mail, icon: assets/mail.svg, color: "#2563eb", roles: [Comms User] }
    - { id: chat, route: chat, title: Chat, icon: assets/chat.svg }
    - { id: meet, route: meet, title: Meet, icon: assets/meet.svg, compact: true }
    - { id: calendar, route: calendar, title: Calendar, icon: assets/calendar.svg, aliases: [agenda] }
  shell.open:                     # which screen shows a record
    - { entity: CommsEvent, path: "calendar/events/{id}" }
  shell.create:                   # the bar's Create menu
    - { label: Email, path: "mail/compose" }
    - { label: Event, path: "calendar/new", entity: CommsEvent }
  shell.intent:                   # what this app does for others (§11.3)
    - { action: compose, type: email, path: "mail/_intent/compose" }
    - { action: schedule, type: event, path: "calendar/_intent/schedule" }
    - { action: share, type: record, path: "chat/_intent/share" }
  shell.sidepanel:                # a panel any app's bar can open (§11.4)
    - { id: chat, title: Chat, path: "chat/_panel" }
  shell.search:                   # results that are not records
    - { id: mail, endpoint: /api/ext/comms/search }
  shell.notification:             # types people switch per channel
    - { id: mention, label: Someone mentions you, default: [inapp, push] }
  shell.work:                     # items for Home's "My work"
    - { id: invitations, endpoint: /api/ext/comms/work }
  shell.card:                     # previews of its records elsewhere (§11.6)
    - { entity: CommsEvent, endpoint: /api/ext/comms/cards/event }
  ui.widget:
    - { id: agenda, title: Today, entry: ui/dist/widget-agenda.html, size: [4, 3] }
extension_points:
  comms.compose.attachment:       # sources of attachments in the composer
    description: Where the composer can attach files from
```

Rules checked when the store publishes and when a cluster makes a release available:
* **Routes** match `[a-z][a-z0-9-]{1,30}`, are unique in the cluster among available releases and their
  aliases, and are never reserved. Reserved now: `api ws files mcp metrics oauth open data admin store
  account portal accounts login logout s app home sw.js robots.txt favicon.ico manifest.webmanifest`,
  and every name starting with `_` or `.`. The core only ever adds `_` names or nests under its own,
  so a new core page never collides with an installed app. An operator may remap an app's route for
  the cluster; apps read their routes from the bootstrap (rule 2).
* **`ui.dist`** has an `index.html` that parses as HTML with one head and one body; built with a
  relative base; scripts only as modules from its own files; no inline scripts, no `<base>`, no remote
  script or style URLs; hashed names for assets; within the store's size limit; messages for the
  default locale.
* **`shell.open`, `shell.create`, `shell.card`** name entities of the app's own packages, or of
  packages it overlays.
* A release without `ui` may contribute `shell.app` entries that open `/data/` views (a records-only
  app, Salesforce-style), or no entry at all (connectors, localisations, background apps).
* A child that lives inside its parent declares no `shell.app`; it contributes to the parent's points
  (`drive.editor`, `erp.module`) and is listed under the parent in the store.

### 10.2 Entry pages and the bootstrap

`GET /crm/deals/42` on a workspace host:

1. Host → workspace, or refusal (§7.2).
2. First path segment → app; a reserved name never gets here.
3. Workspace suspended → status page. App not installed → status page (admins get Install). App
   upgrading → status page with progress that refreshes itself.
4. No session → sign-in, then back to the exact path (`next` must be a path on the same host). A guest
   or portal person → their own pages. No access → status page with "Request access".
5. The release for this workspace (installed, pinned or canary) → its stored entry template, split
   once when the release was made available.
6. The response is the app's `index.html` with, in `<head>`, the bootstrap as
   `<script type="application/json" id="ferrum-bootstrap">` (`<` escaped), the theme
   (`/_shell/theme/{hash}.css`) and the runtime (`/_shell/v1/runtime.js`), and `<ferrum-bar>` first in
   `<body>`. Headers: a fixed CSP per release and host (no nonce: the bootstrap is data, not script);
   `Cache-Control: private, no-cache` with an ETag; a `Speculation-Rules` header for the launcher;
   `Cross-Origin-Opener-Policy: same-origin-allow-popups` (companion windows and sign-in popups keep
   their opener).
7. Paths under the route that the app does not know get the same page; the app's router shows its own
   "not found".

The bootstrap (version 1, a few KB, the same shape for every app):

```json
{ "v": 1,
  "workspace": { "id": "…", "name": "Acme", "host": "acme.ferrum.cloud", "region": "eu-1" },
  "person": { "id": "…", "name": "Ana", "kind": "member", "admin": false, "actor": null },
  "app": { "id": "crm", "version": "1.4.2", "release": "sha256:…", "routes": { "crm": "/crm/" }, "entry": "crm" },
  "host": { "kind": "page", "shell": "1.7.0" },
  "api": { "base": "/api/", "live": "/ws/live", "auth": "cookie" },
  "locale": { "language": "en-GB", "timezone": "Asia/Kolkata", "dir": "ltr" },
  "theme": { "appearance": "system", "hash": "…" },
  "company": "…",
  "flags": {} }
```

Launcher entries, unread counts and contributions are not inlined: the bar runtime fetches
`/api/shell` (ETag) once and keeps it in its worker, so the bootstrap stays small and identical in
shape for every app.

If the app does not start (a script fails to load, or it never calls `shell.ready()` within ten
seconds), the runtime shows "CRM did not start. Reload · Report", reloads by itself at most once, and
reports the release id.

### 10.3 The shell API

| Capability | What it does | Fallback when missing |
|---|---|---|
| `navigate` | go to another app, a record (`{entity, id}`) or a path; the host decides how (a load, a frame switch, a native screen) | `location.assign` |
| `title`, `breadcrumbs`, `actions` | the app's title, trail and actions in the bar | `document.title` |
| `badge` | the app's unread count, on the launcher and the installed app's icon (Badging API) | none |
| `intent`, `canIntent` | start an intent and await its result (§11.3) | the button is hidden |
| `sidepanel` | open a side panel with context | a link to the app |
| `companion` | open or focus a window that outlives page loads (a call, a softphone) | a new tab, with a warning |
| `toast`, `dialog` | messages and dialogs in the host's top layer (needed inside frames) | the kit's own |
| `leaveGuard` | ask before leaving with unsaved changes, for shell navigations and closing | `beforeunload` while dirty |
| `session` | events (signed out, switched account), `reauthenticate()`, `stepUp()` | reload to sign-in |
| `theme`, `locale` | current values and changes | bootstrap values |
| `uploads` | hand an upload to the runtime so it survives navigation (§12.6) | upload in the page |
| `assistant` | open the assistant with context (this record, this selection) | none |
| `help` | open help on a topic | a link |
| `ready` | the app has rendered: start-up check and timing | — |

Each capability has `supports()`; v1 only gains capabilities, never changes one. `@ferrum/client`
exposes them as plain functions that fall back by themselves, so app code has one path.

Hosts that implement the API:

| Host | Where | Bar | Built |
|---|---|---|---|
| page | a top-level tab or installed app window | the runtime draws it | U1 |
| frame | intent dialogs, slots, side panels | the parent's | U1 (dialogs, slots), U4 (panels) |
| companion | call and softphone windows | compact | U4 |
| native | a mobile or desktop shell's web view | native | later |
| test | `@ferrum/testing`, for app CI and component stories | a fake | U1 |
| frame host, foreign suite | a one-window mode; a tab inside another suite | the host's | later, if asked |

### 10.4 Libraries

Ferrum-Frontend becomes an npm workspace that publishes:

| Package | Contents | Comes from |
|---|---|---|
| `@ferrum/kit` | design tokens, CSS, React components, icons, layout (`AppNav`, `FamilyNav`, page header), accessibility helpers | `web/src/ui`, `web/src/components/ui`, `styles.css` |
| `@ferrum/client` | bootstrap, API client (cookie sessions, errors, retries), version-skew handling, live channel client, shell API with fallbacks, permissions, formatting, i18n, uploads | `lib/api.ts`, `meta.ts`, `live.ts`, `format.ts`, `prefs.ts`, `files.ts` |
| `@ferrum/records` | the metadata renderer: list, form, report, filters, child tables, actions, import | `ListView`, `FormView`, `list/`, `ReportView`, `lib/views.ts`, `filters.ts`, `formLayout.ts` |
| `@ferrum/bridge` | frames, slots, intents and panels, host side and frame side | `lib/screens.ts`, ferrum-sdk's `@ferrum/ui` |
| `@ferrum/testing` | a fake bootstrap and shell, contract tests | — |

* Semver, one version line, a changelog; apps pin a range. The bar and the theme are not in these
  packages (D4, D7), so an old app build still looks current.
* `@ferrum/records` gives CRM, Helpdesk or HR complete lists and forms for free; they spend their
  effort on the screens that make them a product.
* Any framework may be used: the contract is files, the bootstrap, the shell API and the HTTP API.

### 10.5 Where code lives and how UI files reach a server

* Shell, Data and the `@ferrum/*` packages: Ferrum-Frontend (`apps/shell`, `packages/*`). ERP's UI:
  Ferrum-Frontend `apps/erp` (ERP's packages are in the Ferrum repository).
* Every other app: `ui/` in its own repository, built and tested in its CI, versioned with its backend.
* Store apps (WebAssembly, metadata): `ui/dist` inside the signed bundle, as screens are today.
* Compiled-in apps (built U4a): the app's `ui/dist` is committed and embedded in its crate (`UI`, by
  its build script, as its packages are); at boot every replica registers it as the UI release of the
  version it was compiled with, idempotently. The app's CI checks that `dist/` was built from `ui/` as
  it is (`dist/.source`, a digest of the sources), so the crate never carries stale files. Nothing to
  collect or fetch when an image is built, no network at boot, and the interface always matches its
  backend. An operator can run another UI release without rebuilding the image (`FERRUM_UI_DIR`,
  registered after, wins for the same version). The cost: built files in the app's git history.
* Until `@ferrum/*` are on npm, an app's `ui/` builds with Ferrum-Frontend's toolchain and dependency
  set from the sibling checkout (`packages/build`: `cli.mjs build|check|typecheck|test`, `vite.mjs`),
  as app crates build against the sibling platform checkout; libraries resolve as Ferrum-Frontend's,
  so an app and the kit share one React and one query client.
* Storage: local disk on a single node, object storage in a cluster, object storage behind a CDN in the
  cloud.
* Ferrum-Frontend's nginx image goes at the end of U4: the server answers every page; an edge proxy
  or CDN may cache `/_ui/` and `/_shell/`.

### 10.6 Templates, dev loop and checks (ferrum-sdk)

* `ferrum-app new crm --ui`: Vite, React, TypeScript, the packages, a bootstrap reader, Playwright.
* `ferrum-app dev --ui`: on a development server (`FERRUM_DEV_UI=1` only), the core sends `/{route}/`
  to the local Vite server behind the real session, bar and bootstrap, with hot reload.
* `ferrum-app check`: manifest, routes, entry page rules, size budgets (§12.9), CSP origins, messages
  for the default locale, an automatic accessibility pass.
* Contract tests from `@ferrum/testing`, run in each app's CI in Chromium, Firefox and WebKit: the app
  starts on a fake bootstrap, calls `ready`, handles sign-out and version skew, and its intent handlers
  answer their schemas.

## 11. Apps inside and alongside other apps

### 11.1 Five ways

| Way | What runs where | Used for | Example |
|---|---|---|---|
| Navigation | the other app's own page | going to work that app owns | Drive's navigation shows Docs; CRM's "Open in ERP" |
| Data contributions | the host app renders what the other declares | actions, menu items, editors, templates, fields (overlays), settings | Browser editors in Drive's "Open with"; a connector's fields on SalesOrder |
| Frames | the other app's page inside the host page: same-origin for trusted apps, sandboxed for the rest | tabs, panels, widgets, side panels | Sign's "Signatures" tab on a CRM deal; a third-party map panel |
| Intent dialogs | the handler's page in a dialog frame, returning a result | pick, create, compose, share, preview, schedule | Mail attaches a Drive file |
| Remote rendering (later) | extension code in a worker; the host renders kit components | small third-party extensions that should look native | a rating widget in Helpdesk |

No way shares JavaScript modules between apps at run time.

### 11.2 Links: `/open/{entity}/{id}`

* Goes to the screen of an app that shows the entity and that the person may open; else `/data/…`;
  else "no access" with "Request access" (Drive has requests for access today).
* Several apps show the same entity (Customer in CRM and ERP): the link's origin wins
  (`/open/Customer/42?via=crm` from inside CRM), then the person's last choice for that entity, then
  `priority`, then install order. "Open in…" offers the others.
* Used by notifications, emails, chat, search, the assistant, cards and other apps. An app links to
  its own screens directly and to anything else through `/open` or `shell.navigate({entity, id})`.
* Inside an app, the kit's router treats any path outside the app's own routes as external and
  navigates the document, so a client-side link never lands in the wrong app (the multi-zone mistake).

### 11.3 Intents

Cross-app actions without shared code, like the Drive Picker:

| Intent | Example | The handler shows |
|---|---|---|
| `pick` | Mail attaches a Drive file; CRM chooses a contact | its picker page |
| `create` | CRM's "Create invoice" for a deal | ERP's new-invoice page, prefilled |
| `compose` | Helpdesk emails the customer | Mail's composer |
| `share` | send a record to a chat channel | Chat's share page |
| `preview` | open a file without leaving CRM | Drive's viewer |
| `schedule` | book a meeting from a deal | Calendar's scheduler |

* `shell.intent({action, type, data})`. The shell finds handlers in installed apps' `shell.intent`
  contributions that the person may use: one is used; several give a chooser with "always use".
* The handler's page opens in a dialog frame (same-origin for trusted apps, sandboxed otherwise),
  receives `data` over a MessageChannel and returns a typed result (a file id, a record id, "sent") or
  "cancelled".
* Payloads are JSON Schemas per `(action, type)`, versioned; the platform owns the common types
  (`file`, `contact`, `email`, `event`, `record`), apps own theirs.
* No handler: `canIntent` is false and the button is hidden, never broken; an API call for it answers
  `no_handler`.
* A "Send email" button in CRM needs neither a dependency on Comms nor a connector.
* The assistant uses apps' MCP tools (today) to act; intents are for people.

### 11.4 Slots and side panels

* An app declares slots in its UI (`crm.record.tab`, `erp.invoice.panel`); other apps fill them.
  A trusted filler gets a same-origin frame with the person's session (fast, looks native with the
  kit); any other gets today's sandboxed frame and bridge.
* The platform's slots (`ui.page`, `ui.widget`, `ui.panel`, `ui.settings`) stay, shown by the Data
  view and Home.
* Frames size themselves through the bridge (ResizeObserver), load when they scroll into view and have
  maximum sizes.
* **Side panels** (`shell.sidepanel`): panels the bar opens in any app: Assistant, Chat, Calendar,
  Tasks. Like Google's side panel, a panel loads again on each page and restores its state from the
  server; one at a time; resizable; on a narrow screen it opens the app instead.

### 11.5 Families

* An app with `parent` and its own `shell.app` entries is a family member. The launcher groups a
  family; every member draws the same family navigation (the kit's `FamilyNav`, from `/api/shell`), so
  Drive and Docs look like one app with two sections while they ship and version on their own; moving
  between them is a page load, as from Google Drive to Docs.
* A child without a UI of its own is drawn by its parent through the parent's extension point
  (Browser editors in Drive).
* **Workspace** is Drive (files, sharing, Office editing; launcher entries Drive, Sheets, Slides and
  PDF, each opening Drive filtered to its kind with its "New…" button) and Docs (live documents).
* A suite is something else: an install bundle (§13). A family is navigation.

### 11.6 Cards

A card is a preview of a record that renders anywhere: on hover over a link, as a chat unfurl, in
search results, notifications, email digests and the mobile app. It is JSON (title, subtitle, a few
fields, an image, actions as intents or links), served by an app's `shell.card` endpoint or derived
from metadata (the title field and the fields shown in lists), always permission-checked. Defined
once, drawn by every host, as Google's add-on cards are.

### 11.7 Events and connectors

* Apps publish events (outbox, webhooks, plugin events, as today); others subscribe.
* Behaviour that needs both apps' data (a won deal becomes a sales order; payroll posts journal
  entries) lives in a **connector** app that requires both, is offered when both are installed and is
  part of suites.

### 11.8 Widgets, My work, search, notifications

Apps contribute Home widgets (`ui.widget`, today), "My work" items (`shell.work`), search providers
(`shell.search`) and notification types (`shell.notification`); §12.8 describes how the shell uses
the last two.

## 12. Platform services

### 12.1 Delivering UI files

* **Content-addressed.** A UI release is stored once under `ui/{app}/{digest}/` and served at
  `/_ui/{app}/{digest}/…`: `immutable`, precompressed (brotli, gzip) when stored, public (no data in
  them). Every replica and any CDN serves every version.
* **Releases and pointers**: `ui_release` (app, digest, files, last used) and a per-workspace pin
  (`tenant_ui_pin`) for rollbacks and canaries; normally a workspace runs its installed release.
* **Retention**: files stay 30 days after no workspace uses them, so open tabs keep loading chunks.
* The shell's own files: `/_shell/v1/…` (the newest v1) and `/_shell/1.7.0/…` (pinned, for rollback
  and support). Theme CSS per workspace is named by the hash of its branding settings and cached
  forever.

### 12.2 The bar runtime

* `/_shell/v1/runtime.js`: small (budget 40 KB compressed), no framework. It defines `<ferrum-bar>` in
  a shadow root (app CSS cannot break it, its CSS cannot leak), installs `ferrum.shell`, and runs on
  every page: launcher (Spaces, pins, search), global search, Create, notifications, side panels,
  assistant, account menu, companion indicator, upload tray, live channel client, version checks,
  offline state, impersonation banner.
* Layout: it sets `--ferrum-bar-height`, which the kit's layout uses; a compact mode for full-screen
  apps (POS, editors, meetings) keeps only the launcher and account; on phones a bottom-sheet launcher.
* Accessibility: a skip link to the main content, landmarks, full keyboard use, focus on the page's
  heading after a load. The bar reserves a few shortcuts (search, launcher, notifications, help); apps
  own the rest.
* If the runtime fails to load, `@ferrum/client` draws a minimal header (Home, account) and the app
  keeps working.

### 12.3 Sessions and sign-in

* **Cookie** `__Host-ferrum` (HttpOnly, Secure, SameSite=Lax, Path=/): a signed token naming a
  `user_session` row (§9.2).
* **CSRF**: a cookie-authenticated request with an unsafe method needs `Sec-Fetch-Site: same-origin`;
  `same-site` and `cross-site` are refused; without that header the `Origin` must equal the host.
  WebSocket upgrades need `Origin` equal to the host. Bearer-token requests are exempt (nothing ambient
  to abuse). The sign-in and OAuth forms keep their own tokens. About thirty lines, no new dependency.
* **Shapes A and A′**: sign-in on the host. **B and C**: a host without a session redirects to
  `accounts.{base}` with `prompt=none` first (a top-level redirect, never a hidden frame, since Safari
  and Firefox block third-party cookies), gets a code and sets its own cookie; a person already signed
  in sees no page.
* **Sign-out**: of this host, or everywhere (every session of the identity is revoked; other hosts
  notice on their next request). Every tab of every app on the host hears it on a BroadcastChannel and
  shows sign-in. Sign-out sends `Clear-Site-Data: "cache"` but never `"storage"` (§16, offline).
* **SSO** and outside OAuth callbacks use the fixed hosts of §7.2.
* **Passkeys** (U6): the relying party is the host (A) or the accounts domain (B, C); custom domains
  sign in through accounts.
* **Bearer tokens** for API, MCP, mobile and CLI, and app passwords for CalDAV and CardDAV, are
  unchanged.
* **App tokens for isolated UIs**: JWTs of a few minutes, audience the app, limited to the app's
  declared permissions within the person's rights; minted over the bridge (sandboxed frames) or by OAuth
  code + PKCE (shape D); refreshed before expiry; refused on admin and sign-in endpoints.

### 12.4 Live channel

* One socket per browser and host: `@ferrum/client` runs `/ws/live` in a SharedWorker that multiplexes
  topics for every tab of every app; one socket per tab where SharedWorker is missing (older Chrome for
  Android).
* Reconnect with jittered backoff and resume from the last event id, so a replica restart or an upgrade
  loses nothing; the server sheds a client that lags.
* Keeping sockets out of pages also keeps pages eligible for the back/forward cache.
* Fan-out stays PostgreSQL NOTIFY until measurements say otherwise; architecture.md's NATS step is next.

### 12.5 Service workers, offline, push

* **Core worker** at `/sw.js`, scope `/`: Web Push (VAPID) and notification clicks (to `/open/…`).
  It has no fetch handler, so it adds nothing to page loads.
* **Offline apps** (POS, scanning, field service) register their own worker at `/{route}/sw.js`; the
  more specific scope controls their pages. They cache their release and the last bootstrap, and must
  handle sign-out and a workspace mismatch.
* **Kill switch**: for routes of apps that were uninstalled, or that the person may no longer use, the
  server answers `/{route}/sw.js` with a worker that clears its caches, unregisters and reloads its
  pages. Workers also check `/api/shell` when they update.
* **Installable apps**: a suite manifest at `/` and optional per-app manifests (`/mail/`, with
  `id: /mail/`); nested scopes are tested in U0 in Chrome, Edge and Safari.
* Unread counts go to installed apps' icons through the Badging API.

### 12.6 Work that outlives a page

| Need | Where it lives | Behaviour |
|---|---|---|
| A call or meeting while working in other apps | a companion window (`shell.companion`, one per kind, named), with Picture-in-Picture opened from that window | the bar in every app shows "In a call · Return"; closing that window ends the call; on phones the call is full screen and leaving it asks first |
| A softphone (a telephony app) | a companion window, the same mechanism | incoming calls through push and the bar |
| Uploads | the bar runtime: files queued in IndexedDB, multipart to object storage, resumed by the next page of any app | a tray in the bar; switching apps is fine; closing the last tab asks first |
| Exports, imports, bulk changes, long reports | server jobs (today's bulk and outbox) | a notification when done |
| Assistant conversations, drafts | the server (threads, draft records), plus sessionStorage | reopen where they were, in any app |
| Timers (timesheets) | the server (start time) | a status item in the bar, contributed by the app (later) |

`window.open` must run inside the click that asks for it; if a popup is blocked, the call opens in the
current tab with a warning that leaving ends it.

### 12.7 Access and entitlements

* One function decides whether a person may use an app: the app is enabled, the person's kind allows
  it, they are an admin or hold one of its roles, and the entitlement check passes ("true" until seats,
  plans and trials exist; they plug in there without touching callers).
* Cached per person, invalidated on role and app changes; used by `/api/shell`, entry pages, `/open`,
  intents and the app's `/api/ext/{app}` endpoints.
* Admin's **App access** page grants apps' roles to teams or people; no new permission model.

### 12.8 Search and notifications

* The kernel searches entities the person can read (permission-filtered, as lists are); apps add
  providers (`shell.search`) for what is not an entity (mail, chat, file contents). Fan-out with a time
  budget (300 ms per provider, results stream in), ranked by kind and recency. PostgreSQL full-text
  first; a search service (Tantivy or Meilisearch) fed from the outbox when a workspace outgrows it,
  still filtered at query time.
* Apps declare notification types (`shell.notification`); each person chooses per type: in-app, email,
  push or off; quiet ones go into an email digest. One inbox in the bar; items open through `/open`.

### 12.9 Language, branding, accessibility, performance

* Locale is settled once (person, then workspace, then browser) and passed in the bootstrap; apps ship
  their own message catalogues (ICU) in `ui/`; the kit uses logical CSS properties, so right-to-left
  languages work. A missing language falls back to the workspace's, then English.
* Branding per workspace: logo, colours, favicon, email sender, domain; theme CSS generated per
  workspace. Resellers (white label) get the same through the platform's branding settings and, later,
  a shell of their own (D14).
* WCAG 2.2 AA for the shell and the kit; the store's review runs automatic accessibility checks on
  UIs.
* Budgets, checked by `ferrum-app check` and in CI: entry JavaScript ≤ 200 KB compressed, CSS ≤ 50 KB,
  Largest Contentful Paint ≤ 2.5 s on a mid-range phone profile.
* App switching: launcher targets are prefetched on hover, prerendered by Speculation Rules where the
  browser supports them; `@view-transition { navigation: auto }` in the theme; no `unload` handlers
  (the kit's lint rule), sockets in the worker and no `no-store`, so going back comes from the
  back/forward cache.
* React is duplicated per app at first; an import map from the core with `integrity` is the upgrade
  path if measurements say it matters, and compression dictionaries can turn upgrades into deltas.

### 12.10 Security

* **CSP per release**, fixed: scripts from `'self'` only, no inline script, `connect-src`, `frame-src`
  and `img-src` limited to the host and the reviewed origins (including those taken from operator
  settings the manifest names), `frame-ancestors 'self'`, `base-uri 'none'`, `object-src 'none'`;
  Trusted Types reported first and enforced per app once its reports are clean (editors and some
  libraries write HTML); violations shown per app on the admin status page.
* `Cross-Origin-Opener-Policy: same-origin-allow-popups` on app pages, `Cross-Origin-Resource-Policy`
  on `/_ui/`, `X-Content-Type-Options: nosniff` everywhere.
* CSRF and sibling hosts: §12.3 and §7.2. User content: rule 9.
* Redirect targets of `next`, `/open` and OAuth are checked against the host and the workspace's hosts
  (no open redirects). Unknown hosts are refused. `/metrics` is not served on workspace hosts.
* **Blast radius.** A trusted UI with an XSS bug can act as the person in every app on that host. That
  is why trusted means first-party, store-verified or operator-installed, and why CSP and Trusted Types
  are strict; operators who want walls between first-party apps can move them to shape D origins
  (rule 2).
* UI files are inside the signed release digest; the server serves only files of releases it verified.

### 12.11 Observability

* `@ferrum/client` reports JavaScript errors, failed chunk loads, start-up failures and web vitals,
  tagged with app, version and workspace (sampled, no personal data, off where the workspace says so).
* The admin status page shows per app: version and release, errors, CSP violations, slow endpoints.
* Server metrics per app: rates, latency and errors of `/api/ext/{app}`, entry pages and intents.

### 12.12 Versions, upgrades, skew

* Every API call carries `X-Ferrum-UI: {app}@{version}`.
* The platform API is versioned (`/api/v1`); an app's endpoints accept the previous minor of its own UI
  (its CI runs the previous release's UI contract tests against the new backend).
* A call that cannot be served for an old UI answers `409 ui_outdated`; the kit offers "A new version
  is available. Reload" and reloads by itself when the tab is idle.
* A failed dynamic import triggers one version check and at most one reload (a sessionStorage guard
  stops loops).
* While an app's upgrade migrates a workspace, its entry pages show "CRM is being updated" and its API
  answers `503 app_upgrading` with `Retry-After`; the kit waits and retries.
* Rolling platform upgrades: replicas of both versions serve every release; shell v1 runtimes of both
  are compatible; the entry template comes from the release, not from the replica.
* Rollback: pin the previous UI release for one workspace when only the UI is at fault; roll the app
  back through the store when the backend is.

## 13. The empty core: splitting the data model

The kernel already holds what a shell needs (workspaces, people, roles, files, audit, notifications,
settings, outbox). The packages change.

What the code shows today: `core` holds Currency, Uom, Company, FiscalYear, ExchangeRate, CostCenter,
Project, Account, CustomerGroup, Customer, SupplierGroup, Supplier, ItemGroup, ItemTaxRate, ItemUom,
ItemSupplier, Item, Warehouse, PriceList, ItemPrice, TaxRow and TaxTemplate. CRM owns Contact and
Address. `accounting`, `buying`, `selling` and HR's expenses link to Project; HR, payroll, Assets,
Field service, POS, Commerce and CRM's quotations link to accounting or inventory records.

| Package | Holds | Installed |
|---|---|---|
| `core` (shrunk) | Company, Currency, ExchangeRate, Uom | always |
| `contacts` (new, hidden) | Contact, Address (from CRM) | with any app that deals with outside people: CRM, Helpdesk, Comms, Commerce, Bookings, Campaigns, Sign |
| `parties` (new, hidden) | Customer, CustomerGroup, Supplier, SupplierGroup | with CRM, Helpdesk, Commerce, POS, Field service, ERP |
| `products` (new, hidden) | Item, ItemGroup, ItemUom, PriceList, ItemPrice | with CRM, Commerce, POS, Field service, ERP |
| `accounting`, `inventory`, `selling`, `buying`, `manufacturing` | as today, plus Account, CostCenter, FiscalYear, TaxTemplate, TaxRow, ItemTaxRate (accounting), Warehouse and ItemSupplier (inventory, buying) | through the **ERP** app |
| Project | `project_records`, a shared record (§20.4's alternative, chosen in U3: ERP's ledger and budgets depend on it); the Projects app adds the work on it, `projects-erp` its invoicing | with ERP or Projects |
| ERP-only fields on shared records (a customer's receivable account, an item's stock settings) | overlays from the ERP packages, which the kernel supports today | with ERP |

Rules:
* Every entity has one owning package; others overlay it. Shared records live in the lowest package all
  their users need; no app keeps a copy.
* A field that links into another app's package lives in a connector's overlay. Moved fields keep their
  names, so columns and data stay where they are.
* An app depends on the least it needs; anything more is a connector or an intent.
* The split is driven by a generated report (`ferrum task deps-report`): every link, overlay, hook and
  report that crosses packages. CI keeps it at zero unexpected crossings afterwards.

| App | Depends today | After | Connector for the rest |
|---|---|---|---|
| CRM | core, selling (overlays SalesOrder, SalesOrderItem) | core, contacts, parties, products | `crm-erp`: quotation to sales order, the overlays, tax and warehouse fields on quotations |
| Helpdesk | core, selling | core, contacts, parties | `helpdesk-erp` only if a need appears |
| HR, payroll | hr: core, accounting; payroll: hr, accounting | hr: core; payroll: hr | `payroll-erp`: salary postings; `hr-erp`: expense claims to accounts |
| Projects | core, accounting, selling | core, parties | `projects-erp`: project fields on ERP documents, timesheet billing, costing |
| Commerce, POS, Field service, Assets, Bookings | ERP packages | ERP-family apps, `requires: { erp }` | — |
| Comms, Drive, Docs, Forms, Sign, LMS, Campaigns | core (Docs: drive; Campaigns: comms) | as today, plus `contacts` where they address outside people | — |

**Moving existing workspaces.**
* Tables do not move: storage is keyed by entity (`e_{entity}`), not by package.
* A new kernel operation "move entity or field to package" reassigns ownership; its tests cover catalog
  validation, overlays, permissions and roles, numbering, print formats, seeds, personal-data rules and
  workspace customizations of the moved entities (customizations target entities, so they keep
  applying).
* An upgrade step installs `contacts`, `parties`, `products`, `erp`, Projects and the connectors in
  every workspace that has the ERP packages (and Projects with `projects-erp` where Project records
  exist), so their data and screens stay as they were.
* Rehearsed in CI on the demo database and on an imported ERPNext dataset; the old ownership can be
  restored by running the step backwards until the next release.

**Apps and suites.** ERP becomes an app (`id: erp`, requiring its five packages plus `parties` and
`products`; India is its add-on). A suite is an app without packages whose `requires` lists members and
connectors: `workspace` (drive, docs, comms), `business` (erp, crm, helpdesk, projects and their
connectors), `retail` (erp, commerce, pos). Uninstalling a suite asks whether to remove members nothing
else needs (members installed only as dependencies are marked so).

**Every app's interface.**

| App | Interface | Notes |
|---|---|---|
| Shell | own (`/`) | Home, store, admin, account, data |
| ERP | own (`/erp/`) | today's records UI and ERP pages; the POS till moves to POS |
| CRM | own | pipeline board, deals, leads, activities, quotations |
| Helpdesk | own + portal | agent inbox, ticket view; "My tickets" on the portal |
| HR, payroll | own ("People") + self-service | employees, leave, attendance, payroll runs |
| Projects | own | boards, Gantt, timesheets |
| Comms | own, four routes | Mail, Chat, Meet, Calendar; out of `web/src/app/comms` |
| Drive | own, family Workspace | files, sharing, Office editing; entries Drive, Sheets, Slides, PDF; out of `web/src/app/drive` |
| Docs | own, family Workspace | live documents; out of `web/src/app/docs` and `drive/LiveDoc.tsx` |
| Commerce | own + public site | store admin; the storefront on a `site:` host |
| POS | own, full screen, offline | the till |
| Studio | own | builders now in `web/src/app/studio`, `designer`, `FormDesigner`, `Customize` |
| Forms, Sign, Bookings, LMS, Field service, Assets, Campaigns | records-only (`/data/` navigation) first | an own UI when each is next worked on; Campaigns may become a CRM section |
| Third-party apps | isolated full UI, slots, intents | D9 |

## 14. Moving from today's frontend

1. U1: the server learns to serve app UIs; the board example gets a full UI; nothing changes for
   people.
2. U2: Ferrum-Frontend becomes a workspace (`packages/*`, `apps/shell`, `apps/erp`). Today's SPA keeps
   running at `/app/` for workspaces without the `shell_v2` switch.
3. Switching `shell_v2` on for a workspace makes `/` the shell and `/erp/` ERP, and turns `/app/…` into
   redirects; switching it off goes back. Redirect map: `/app/e/{E}/{id}` → `/open/{E}/{id}`;
   `/app/e/{E}` → the list of the app that owns `{E}`, else `/data/{E}`; `/app/r/{name}` → the owning
   app's report, else `/data/`; `/app/mail…`, `/app/chat…`, `/app/calendar…`, `/app/meet…` →
   `/mail/…` and so on; `/app/drive…`, `/app/edit/…` → `/drive/…`; `/app/doc/{file}`, `/app/docs` →
   `/docs/…`; `/app/pos` → `/pos/`; `/app/studio/…`, `/app/customize…` → `/studio/…`;
   `/app/x/{app}/{page}` → that app's route, else `/data/`; `/app/s/…` and `/app/meet/guest/…` → the
   new public link pages.
4. U4: Comms, Drive and Docs move out of `web/src/app/{comms,drive,docs}` into their repositories'
   `ui/`; POS into POS; Studio into Studio.
5. End of U4: `shell_v2` is the default, `/app/` is redirects only, and the nginx image is retired.

## 15. Scale

| Concern | At target scale | How |
|---|---|---|
| Static UI traffic | most bytes | content-addressed files on object storage behind a CDN; app servers never see repeat fetches |
| Entry pages, `/api/shell` | one per app load per person | built from per-workspace caches (installed apps, contributions, theme) and a per-person access cache; ETags; no database query on a warm path |
| Host lookups | every request | in-memory map on each replica, NOTIFY invalidation |
| Contribution lookups | every launcher, intent, `/open` | per-workspace cache from installed manifests (as `contributions` is today) |
| Sessions | 200 000 concurrent | one indexed row per device; last-seen at most every five minutes; validity cached 20 s |
| Identities | one directory per cluster | global table keyed by email, region-pinned; read on sign-in only |
| Sockets | one per browser, not per tab | SharedWorker; NOTIFY fan-out, NATS when one PostgreSQL's notifications are the limit |
| Search | large workspaces | provider budgets; a search service per region when full-text is too slow |
| Certificates | thousands of custom domains | on-demand TLS with an ask endpoint, shared storage, DNS-01 where rate limits bite |
| Regions | residency | a workspace pinned to a region; the accounts directory routes; UI files global |
| Store and catalog | 1 000 apps | catalog cached per cluster; release files immutable; mirrors for air-gapped installs (apps.md §9) |
| Noisy apps | one app's endpoints slow everything | per-app rate limits and timeouts on `/api/ext/{app}`; per-app metrics |
| Launch counts | every entry page | a counter per person and app, batched; feeds "most used" |

## 16. Edge cases

### Opening apps and links

| Situation | Behaviour |
|---|---|
| `/crm/` when CRM is not installed | status page "CRM is not installed"; admins get Install |
| Installed, but the person may not use it | "You do not have access to CRM" and "Request access" to admins |
| A route renamed in a new release | `aliases` redirect the old route |
| Two apps want the same route | refused at publish and at make-available, aliases included; an operator may remap one |
| A future core page needs a name an app uses | cannot happen: the core only adds names starting with `_`, or nests under its own |
| A link to a record whose app is gone | `/open` falls back to `/data/`; if the record is gone, "This item no longer exists" |
| A record shown by several apps | origin, last choice, priority (§11.2); "Open in…" |
| An in-app link to another app's path | the kit navigates the document instead of routing on the client |
| A deep link while signed out | sign-in, then back to the exact path |
| A guest or portal person opens a member URL | sent to their own pages; never sees the launcher |
| Old `/app/…` bookmarks | redirect (§14) |
| One app installed / none | land in it / Home with the first-run picker |
| Very many apps for one person | Spaces, pins, launcher search, most used first |

### Installing, upgrading, removing

| Situation | Behaviour |
|---|---|
| App disabled or uninstalled while tabs are open | its API answers `app_unavailable`; the kit shows the status page; its worker gets the kill switch |
| App upgraded while tabs are open | old chunks still load (§12.1); incompatible calls answer `ui_outdated` and the tab offers a reload |
| An upgrade is migrating the workspace | "being updated" page; `503 app_upgrading` with `Retry-After`; the kit retries |
| An upgrade fails for one workspace | it stays on the old version, backend and UI; the store shows the error; retrying is idempotent |
| A release's UI is broken, its backend fine | pin the previous UI release for affected workspaces from the admin status page |
| The app needs a newer shell API than the server has | install refused with the reason; after a platform rollback, a status page says which platform it needs |
| Parent removed while children are installed | refused, listing the children (platform.md §3.3) |
| A connector's other side removed | the connector is disabled with it and offered for removal |
| Suite uninstalled | members kept unless the admin removes those nothing else needs |
| App needs a service that is not deployed (Mail, Office, Meet) | install succeeds; the app's status says what is missing and its UI shows setup steps, not errors |

### Sessions and identity

| Situation | Behaviour |
|---|---|
| Session revoked on another device | next request 401; every tab on that host shows sign-in |
| Session expires mid-edit | sign-in in a dialog on the page, the request retried, nothing lost |
| Signed in to two workspaces | separate hosts and cookies (B, C); the account menu switches |
| Same email, two workspaces, two passwords (B, C) | one identity; each membership joins after the person proves it once |
| Shape A′ host, a second workspace in another tab | one at a time; signing in to the second switches; other tabs say "you switched to X" and reload |
| Impersonation | a banner in every app; actions audited with both people; time-limited |
| A guest's share is revoked | their page says it is no longer shared |
| Identity provider down | password sign-in only if the workspace policy allows it; admins keep a break-glass sign-in |
| Accounts host unreachable (B, C) | existing sessions keep working; new sign-ins see a clear error; accounts is served by the same cluster, not a separate service |
| Second factor lost | recovery codes or an admin reset (today) |

### Hosts and domains

| Situation | Behaviour |
|---|---|
| A custom domain's DNS removed | unverified after 7 failed daily checks; renewals stop; the subdomain keeps working |
| Workspace renamed | old host redirects for 90 days, then is quarantined for a year |
| Workspace deleted | its hosts are quarantined; links say the workspace no longer exists |
| A browser holds data from an earlier owner of a host | wiped when the workspace id does not match (§7.2) |
| A domain added after SSO or a connector was set up | callbacks use fixed hosts; nothing to re-register |
| A request for an unknown host | refused before any app code |
| Workspace moved to another region | a read-only window while records copy; hosts repointed; the directory updated; open tabs reload |

### Offline, devices, mobile

| Situation | Behaviour |
|---|---|
| POS offline when the session expires | it keeps queuing sales and asks to sign in before syncing; each sale keeps who recorded it |
| Sign-out with unsynced offline data | warned; the data stays (no `Clear-Site-Data: "storage"`) |
| One device, several people (later) | a device session and a PIN switch (§9.2) |
| An installed app the person lost access to | opens the status page; its worker unregisters |
| A browser without SharedWorker, view transitions or prerendering | one socket per tab; plain page loads; prefetch only |
| The app inside a native shell (later) | the native host implements the shell API; the same UI |

### Work across apps

| Situation | Behaviour |
|---|---|
| Moving to another app during a call | the call goes on in its companion window; the bar shows it |
| The popup for a call is blocked | the call opens in the current tab, with a warning that leaving ends it |
| An upload in progress while switching apps | it continues in the next page; closing the last tab asks first |
| The assistant mid-conversation while switching apps | the same thread reopens |

### Versions and failures

| Situation | Behaviour |
|---|---|
| Old app build calling a removed platform endpoint | `/api/v1` stays; removal only in a major, after two minors of warnings |
| A tab open for weeks | its files stay 30 days after last use; after that one reload |
| `runtime.js` cannot load | the kit's fallback header (§12.2) |
| The app does not start | the runtime's recovery message, one automatic reload, a report |
| Replicas on two platform versions during a rollout | both serve every release; shell v1 runtimes are compatible |

### Data model

| Situation | Behaviour |
|---|---|
| An app needs records of an app that is not installed | a connector or an intent; never "is X installed" in code |
| ERP removed from a workspace that has customers | ERP's overlay fields disappear from screens; their data stays (uninstall policy) |
| Entities move packages during an upgrade | tested on two datasets; reversible for one release |
| Customizations of a moved entity | keep applying: they target entities, not packages |

### Security

| Situation | Behaviour |
|---|---|
| A third-party UI tries to read the session | it cannot: opaque origin or an origin of its own, no cookie, short app tokens |
| An XSS bug in a trusted UI | strict CSP and Trusted Types limit it; its reach is the host; shape D isolates further |
| A page on a sibling workspace host forges requests | `same-site` refused on unsafe methods; `Origin` checked on sockets; the public-suffix domain removes the case |
| Uploaded HTML or SVG | served with a sandbox policy or as a download (today) |
| `next` or `/open` used to redirect elsewhere | only paths on the host or the workspace's hosts |
| An app's CSP needs an origin from operator settings | the manifest names the setting; the policy is built from its value |

### Later, designed for now

| Situation | Behaviour |
|---|---|
| A one-window mode (Zoho's goal) | a frame-host adapter for the shell API; apps unchanged |
| Native mobile and desktop apps | a native host adapter; universal links from `.well-known` files per host |
| Ferrum apps inside another suite (a CRM tab in Teams, a Gmail add-on) | a foreign-host adapter mapping that suite's SDK to `ferrum.shell`; cards for add-ons |
| Apps customers build in Studio without code | tenant apps: `shell.app` entries pointing at Data views and Studio pages; exported as metadata packages |
| A publisher that hosts its own UI | shape D with the publisher's origin, OAuth and app tokens; the store reviews the origin |
| Firms serving many workspaces | a partner view on accounts |
| Seats, plans, trials | the access function's entitlement hook |
| Small third-party extensions that should look native | remote rendering in a worker (§11.1) |
| A UI written in Vue, Svelte or plain HTML | allowed: the contract is files, bootstrap and APIs |

## 17. Phases

Each phase ends usable, with tests, docs and a browser check in Chromium, Firefox and WebKit. Until
U4 ends, today's web app keeps working at `/app/` behind the per-workspace `shell_v2` switch, so the
change rolls out and back per workspace.

| Phase | Delivers | Done when |
|---|---|---|
| **U0. Spike** (about a week) | cookie session with Fetch-Metadata CSRF and Origin checks on sockets; entry pages assembled from a stored release (bootstrap, theme, runtime) for two tiny apps at `/crm/` and `/notes/`; `<ferrum-bar>` with the launcher and `ferrum.shell` (navigate, title, intent, ready); one `pick` intent between them; the SharedWorker live channel with the per-tab fallback; a companion window with Picture-in-Picture that survives loads in the main window; back between apps from the back/forward cache (checked with `notRestoredReasons`); a cross-document view transition and launcher prerendering; a suite install at `/` and a Mail install at `/mail/`; the core worker without a fetch handler and the kill-switch worker; `{workspace}.localhost` | `spike-u0-apps-ui.md` with go/no-go per item and engine, and measured load and switch times |
| **U1. Shell platform** | tables (`user_session` with `actor` and `step_up_at`; `tenant_host` with `target`, `primary`, `released_at`; `app_user.kind`; `ui_release`; `tenant_ui_pin`); host resolution (shapes A, A′); entry pages; `/_ui/` and `/_shell/`; theme CSS; `/api/shell`; `/open`; `shell.*` points with validation and reserved names; the access function and App access page; devices page; skew handling; status pages; kill-switch workers; `@ferrum/testing`; `ferrum-app dev --ui` | the board example ships a full UI from its bundle; a person without its role neither sees nor opens it; an upgrade with open tabs passes; CSRF, sibling-host and Host-header tests pass |
| **U2. Frontend split** | `packages/kit`, `client`, `records`, `bridge`, `testing` on npm; `apps/shell` and `apps/erp`; the `shell_v2` switch and the redirect map | a switched workspace loses no page; Playwright suites green; switching back works |
| **U3. Empty core** | the dependency report; `contacts`, `parties`, `products`; the move operation (entities and fields); the ERP app; connectors (`crm-erp`, `projects-erp`, `hr-erp`, `payroll-erp`); suites; the first-run picker; the upgrade step | a new workspace starts with only the shell; CRM installs alone; the demo and ERPNext-import workspaces upgrade with identical data and can go back |
| **U4a. Comms and Workspace** (beside U3: they need only `core` and `contacts`) | Comms (four routes), Drive and Docs with the family navigation; intents `pick` file, `preview` file, `compose` mail, `schedule` meeting, `share`; side panels (Assistant, Chat); Meet in a companion window; uploads in the runtime | they install, upgrade and uninstall with their UI; Ferrum-Frontend holds none of their code |
| **U4b. CRM** (after U3) | CRM's own UI on `contacts`, `parties`, `products`; `crm-erp` screens through slots and intents | CRM works alone and with ERP; `shell_v2` becomes the default; `/app/` is redirects only; the nginx image is retired |
| **U5. The rest** | Helpdesk, People, Projects, POS (offline, own worker), Studio, Commerce admin; Home widgets and My work; notification types and preferences; search providers; cards | §13's interface table done |
| **U6. Many workspaces and hosts** | shapes B and C: identity and memberships, the accounts host and chooser, custom domains with TLS recipes, fixed callback hosts, quarantine, regions in the directory, passkeys | two workspaces on two subdomains and one custom domain, one identity, in the kind cluster |
| **U7. Portal and sites** | `app_user.kind` honoured everywhere, portal sign-in, `shell.portal` pages from Helpdesk and ERP; `site:{app}` hosts with Commerce's storefront | a customer signs in to the portal and sees only their tickets and invoices; the storefront serves on its own host |
| **U8. Third parties** | isolated full UIs (sandboxed full page, shape D origins), app tokens, the store's review of UI bundles (size, origins, accessibility), screenshots; a remote-rendering spike | a third-party sample app with a full UI installs from the store and can neither read the session nor act beyond its permissions |
| **Later, when asked** | device-bound sessions, shared devices, native shells, the frame host, foreign hosts, the partner view, seats and plans | — |

Order: U0–U2 change nothing people see; U3 is the riskiest (data model) and lands before CRM gets its
UI, so CRM builds against the final dependencies; Comms and Workspace do not wait for it; U5 repeats a
proven pattern; U6–U8 are independent of each other and follow demand.

## 18. Risks

| Risk | Answer |
|---|---|
| Apps drift apart in look and behaviour | the bar and theme come from the core at run time; one kit; a UI checklist in the store review |
| A trusted UI's XSS acts as the person in every app on the host | trusted only when first-party, verified or operator-installed; strict CSP and Trusted Types; shape D for anything else |
| Page loads between apps feel slow | inline bootstrap, immutable caching, prefetch and prerender, view transitions, back/forward cache, budgets; the same cost Google and Zoho pay |
| Calls and long tasks break when people switch apps | companion windows, uploads in the runtime, server jobs (D12); tested in U0 |
| Moving entities corrupts workspaces | tables stay put; one tested operation; rehearsed on two datasets; reversible for one release |
| Merging identities joins the wrong accounts | proof per membership, never email alone; audit of every link |
| Inserting the shell into apps' HTML goes wrong | entry templates parsed and split once at make-available, with strict rules on `index.html`; contract tests |
| Version skew between UI, app backend and platform | immutable files for 30 days; endpoints accept the previous UI minor; `ui_outdated`; API majors |
| Browser differences (nested install scopes, SharedWorker on Android, view transitions) | capabilities with fallbacks; U0 measures each in three engines |
| Cookie sessions change sign-in for every client | bearer tokens keep working; cookies are added, not swapped; U0 checks three engines |
| Custom domains and TLS at scale | on-demand TLS with an ask endpoint and shared storage; DNS-01 at volume |
| Sibling workspace hosts are same-site | `same-site` refused; Origin checks; the public-suffix listing (applied for early, it takes time) |
| The rewrite stalls with two frontends | the `shell_v2` switch per workspace; every phase leaves a working product; `/app/` removed only at the end of U4 |

## 19. Not building

| Not building | Would reconsider when |
|---|---|
| Module federation, single-spa or any run-time composition of several apps' code in one page | never planned; only for a measured need that frames and intents cannot meet |
| A frame host by default | people ask for a one-window mode and companion windows are not enough |
| Shape D origins for first-party apps by default | an operator or a security review asks |
| Shared React through an import map | duplicated vendor code measurably slows switching |
| Copies of shared records per app | never |
| A search service | PostgreSQL full-text misses its budget for a workspace |
| NATS | NOTIFY is the measured bottleneck |
| Seats, plans, trials | paid apps exist |
| Path-per-workspace hosting | a self-hoster needs several workspaces at once without wildcard DNS and A′ does not fit |

## 20. Questions for the owner

The plan picks a default for each, so work can start; these are the choices worth a second look.

1. **Cloud domains**: which registrable domain carries workspace hosts (to be listed as a public
   suffix), and which carries accounts and the website.
2. **Workspace family**: Drive and Docs as two UIs with shared navigation (this revision), rather than
   one Workspace UI (the earlier draft).
3. **Shared packages**: `contacts`, `parties` and `products` (this plan), or one `masters` package.
4. **Project**: owned by the Projects app, with ERP's project fields in `projects-erp` (this plan), or
   kept as a shared record.
5. **Data view** in the core as the admins' generic records UI (this plan), or inside Studio.
6. **Self-hosting default**: shape A (this plan), with A′ for those who run several workspaces on one
   host today.

## 21. What this replaces in earlier plans

* platform.md §1 decision 3 ("first-party screens stay in the web app") and §3.4 Tier 2: first-party
  experiences become app UIs served from their releases. Tiers 0 and 1 stay: Tier 1 screens become the
  sandboxed way strangers extend an app, and the bridge gains intents and side panels.
* platform.md §7 "runtime loading of trusted JavaScript (module federation)": still no module
  federation; what is new is that whole app pages from signed releases are served at run time.
* apps.md §12: Ferrum-Frontend holds the shell, ERP's UI and the shared packages; app repositories gain
  `ui/`. apps.md §4's bundle limits need a UI allowance.
* The sidebar built from every module (`Shell.tsx`) and the module cards on Home go away; each app
  builds its own navigation.

## 22. Changes from the earlier drafts

* Work that must survive a switch between apps (calls, softphones, uploads, assistant) has a home
  (D12, §12.6); Picture-in-Picture alone cannot do it.
* The shell is put into apps' pages by the server (D4); the bootstrap is a JSON block, so the CSP
  needs no nonce and entry pages stay out of `no-store`, keeping the back/forward cache.
* One host-agnostic shell API with capabilities and fallbacks (D5, §10.3), so the same UI runs in a
  tab, a frame, a companion window, a native shell or a test.
* CSRF by Fetch Metadata, refusing `same-site`, because sibling workspace hosts are same-site; Origin
  checks on sockets; a public-suffix domain for workspace hosts (§7.2, §12.3).
* Hosts gain a target (shell, portal, an app's public site), a primary host, redirects, quarantine and
  fixed hosts for outside callbacks (§7.2, §7.3); shape A′ keeps today's shared-host sign-in.
* Identity across workspaces is spelt out: a cluster identity with memberships, linking by proof,
  regions (§9.3); sessions gain impersonation, step-up, re-sign-in without losing work, device binding
  and shared devices later (§9.2).
* Families share navigation, not a bundle: Drive and Docs keep their own UIs and releases (§11.5).
* Five ways for one app to appear in another (§11.1); side panels and cards are new (§11.4, §11.6).
* The data split follows the code: Contact and Address move from CRM to `contacts`; `parties` and
  `products` are separate; Project goes to the Projects app with ERP's project fields in a connector;
  a dependency report drives and then guards the split (§13).
* How compiled-in apps' UIs reach a server: artefacts pinned by digest (§10.5).
* Comms and Workspace no longer wait for the data split (U4a); the edge-case catalogue covers the
  later shapes too (§16).

## 23. Sources

* Zoho One 2025 redesign: https://www.computerworld.com/article/4093763/zoho-revamps-zoho-ones-ui-to-focus-on-work-not-apps.html,
  https://www.bcstrategies.com/post/zoho-one-pushes-business-software-beyond-apps-toward-a-unified-operating-system,
  https://www.businesswire.com/news/home/20251118538194/en
* Zoho data centres and sign-in: https://help.zoho.com/portal/en/kb/accounts/sign-in-za/articles/signing-in-to-multiple-accounts,
  https://www.zoho.com/developer/oauth/multi-dc-support.html
* Frappe apps screen: https://docs.frappe.io/framework/user/en/apps-page,
  https://docs.frappe.io/framework/user/en/desk/navigation/desktop
* SAP Fiori intent-based navigation: https://help.sap.com/saphelp_uiaddon20/helpdata/en/3a/35ab684174415899c98da4931bd6c1/content.htm,
  https://learning.sap.com/courses/ui-development-with-sap-fiori/implementing-the-crossapplicationnavigation-service_fb4e7db5-b59c-43a4-8a41-8a1f34a0bda1
* TeamsJS capabilities across hosts: https://learn.microsoft.com/microsoftteams/platform/m365-apps/teamsjs-support-m365,
  https://github.com/OfficeDev/microsoft-teams-library-js/wiki/Library-Architecture
* Microsoft's single domain: https://learn.microsoft.com/microsoft-365/enterprise/cloud-microsoft-domain
* Google Workspace add-on cards: https://developers.google.com/workspace/add-ons/concepts/card-interfaces
* Salesforce utility bar softphones: https://trailhead.salesforce.com/content/learn/projects/set-up-the-service-console/add-the-softphone-utility-to-your-app
* Shopify App Bridge and Polaris versioning: https://shopify.dev/docs/api/app-bridge-library/reference,
  https://shopify.dev/docs/api/app-home/latest/web-components/versioning.md,
  https://shopify.dev/concepts/apps/building-embedded-apps-using-session-tokens
* Remote rendering: https://shopify.engineering/remote-rendering-ui-extensibility, https://github.com/Shopify/remote-dom,
  https://developers.hubspot.com/docs/platform/ui-extensions-overview
* Atlassian Forge UI: https://developer.atlassian.com/platform/forge/ui-kit/overview/
* Next.js multi-zones: https://nextjs.org/docs/15/pages/guides/multi-zones
* Vercel skew protection: https://vercel.com/docs/concepts/deployments/skew-protection
* QuickBooks Online Accountant: https://quickbooks.intuit.com/learn-support/en-us/help-articles/how-to-switch-between-companies/00/261466
* Document Picture-in-Picture closes with its opener: https://github.com/WICG/document-picture-in-picture,
  https://chromium.googlesource.com/external/w3c/web-platform-tests.git/+/HEAD/document-picture-in-picture/closes-on-navigation-or-destroy.https.html
* Back/forward cache and `no-store`: https://developer.chrome.com/docs/web-platform/bfcache-ccns,
  https://developer.mozilla.org/docs/Web/API/Performance_API/Monitoring_bfcache_blocking_reasons
* SharedWorker on Android: https://groups.google.com/a/chromium.org/g/blink-dev/c/pS1PDOa69CU
* Cross-document view transitions: https://developer.chrome.com/docs/web-platform/view-transitions/cross-document
* Prerendering: https://developer.chrome.com/blog/prerender-pages/
* CSRF with Fetch Metadata: https://simonwillison.net/b/9053, https://docs.rs/tower-csrf
* Device Bound Session Credentials: https://blog.google/security/protecting-cookies-with-device-bound-session-credentials/,
  https://workspaceupdates.googleblog.com/2026/05/prevent-account-takeovers-with-DBSC-now-generally-available-in-the-Chrome-browser-for-Windows.html
* Passkeys across domains: https://web.dev/articles/webauthn-related-origin-requests
* Clear-Site-Data: https://developer.mozilla.org/docs/Web/HTTP/Headers/Clear-Site-Data
* Import maps with integrity: https://nesbitt.io/2026/01/19/importmap-lock
* Compression dictionaries: https://developer.chrome.com/blog/search-compression-dictionaries
* Several installable apps per origin: https://developer.chrome.com/blog/pwa-manifest-id, https://developer.chrome.com/docs/capabilities/scope-extensions
* Custom domains with on-demand TLS: https://caddy.community/t/serving-tens-of-thousands-of-domains-over-https-with-caddy/11179
* One socket across tabs: https://pusher.com/blog/reduce-websocket-connections-with-shared-workers
* Service worker scopes: https://developer.chrome.com/docs/workbox/service-worker-lifecycle
* Nextcloud's component library: https://npmjs.com/package/@nextcloud/vue
