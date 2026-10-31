# Plugins: turning Ferrum into any platform

A plugin is a WebAssembly module plus a manifest. It can validate and enrich documents, bring its
own entities, serve HTTP routes (signed-in or public), hold WebSocket connections to devices,
react to events, run on a schedule and call declared HTTPS services — all inside a sandbox, with
only the permissions an administrator approved. That is enough to turn Ferrum into an EV charging
system (OCPP over WebSocket, OCPI over HTTPS), an IoT platform, a payment or telephony gateway,
without changing Ferrum itself.

`plugins/examples/device-gateway` is a complete example (devices over WebSocket, commands from
users, readings as records, a public status route, an event subscription, a schedule and a
configurable hook); `plugins/sdk` is the Rust SDK it uses.

## Writing one (Rust)

```rust
use ferrum_plugin_sdk::{plugin, Request, json, Value, data, kv, send};

plugin!(handle);

fn handle(req: Request) -> Value {
    match req.kind() {
        "socket" => json!({"send": "{\"type\":\"accepted\"}"}),
        "http" => json!({"json": {"hello": req.user_email()}}),
        _ => json!({}),
    }
}
```

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

Unit tests run natively (`cargo test`): `ferrum_plugin_sdk::testing::host(|op, args| …)` answers
the plugin's host calls (`Ok(value)` or `Err((code, message))`), so a test can feed an input to
the handler and check every record, key-value and HTTP call it made. The Commerce app's payment
gateways (`gateways/` in its repository: Razorpay and Stripe) are complete examples.

Any language that compiles to a core WebAssembly module works: the contract is below.

## Installing

`POST /api/admin/plugins` (administrators) with the manifest fields plus `wasm_base64`. Installing
again with the same `name` upgrades it in place. `PUT /api/admin/plugins/{id}/enabled`
`{"enabled": false}` switches it off at once; `PUT /api/admin/plugins/{id}/config` `{"config": {...}}`
replaces its settings and secrets (listings only show the keys); `DELETE /api/admin/plugins/{id}`
removes it with its private data (records it created stay).

## Manifest

| Field | Meaning |
|---|---|
| `name`, `version`, `description` | `name`: 2–40 lower-case letters, digits or `-` |
| `config` | Settings and secrets, delivered in every input as `config` |
| `entities` | Entity definitions it brings (masters or documents, names start with `X`), stored without DDL |
| `permissions` | `[{entity, read, write, create, delete, submit, cancel}]` — its service user's role, and the most it may do on a user's behalf |
| `hooks` | `[{entity, stage}]` with stage `before_validate`, `validate`, `before_submit`, `on_submit`, `before_cancel`, `on_cancel`, `before_delete`; entity `*` for all |
| `routes` | `[{path, public}]`; `/x` or `/x/*`. Signed-in: `/api/ext/{plugin}{path}`; public: `/api/public/{tenant}/{plugin}{path}` |
| `sockets` | `[{path, public, subprotocols}]` at `/ws/ext/{tenant}/{plugin}{path}`; clients must offer one of the subprotocols |
| `subscriptions` | Outbox topics: `SalesInvoice.submitted`, `SalesInvoice.*`, `plugin.other.*`, `*` |
| `schedules` | `[{name, every_seconds}]` (at least 60) |
| `http_hosts` | Hosts it may call: `api.example.com`, `*.example.com` |
| `fuel_limit`, `memory_limit_mb` | Per-call CPU budget (default 50 M) and memory cap (default 32 MB) |

## Contract

The module may import only `ferrum.host_call(ptr, len) -> i64` and must export `memory`,
`ferrum_alloc(len) -> ptr` and `ferrum_hook(ptr, len) -> (ptr << 32 | len)`. Every call is a
fresh instance; input and output are UTF-8 JSON.

| `kind` | Input (besides `kind`, `config`) | Reply |
|---|---|---|
| `hook` | `entity`, `stage`, `doc`, `user` | `{}`, `{"patch": {...}}` (before_validate), `{"error"}` or `{"errors": [{field, message, row?}]}` |
| `http` | `method`, `path`, `query`, `headers` (no credentials), `body`, `user` (null when public) | `{"status", "headers", "body"}` or `{"status", "json"}` |
| `event` | `topic`, `event` (the outbox payload) | `{}`; an error makes the event retry |
| `schedule` | `name` | `{}` |
| `socket` | `event` (`connect`/`message`/`close`), `connection`, `path`, `query`, `subprotocol`, `message`, `user` | `{"send": text or [texts], "close": bool}` |

**Identity.** Hooks run inside the document's transaction as the user; signed-in routes and
sockets run as the user. In both cases the plugin is also limited to its declared permissions.
Public routes, public sockets, events and schedules run as the plugin's own service user, whose
role is exactly its declared permissions. Everything it changes lands in the audit trail.

## Host calls

`host_call` takes `{"op", "args"}` and returns `{"ok": value}` or
`{"error": {"code", "message", "fields"?}}`.

| `op` | `args` → result |
|---|---|
| `data.get` | `{entity, id}` → record |
| `data.query` | `{entity, filters, fields?, sort?, limit?, cursor?}` → `{rows, next_cursor?}` |
| `data.insert` / `data.update` / `data.delete` | `{entity, data}` / `{entity, id, data, expected_version?}` / `{entity, id}` |
| `data.submit` / `data.cancel` / `data.action` | `{entity, id}` / `{entity, id}` / `{entity, id, action, params}` |
| `kv.get` / `kv.set` / `kv.delete` / `kv.list` | private per-tenant storage: `{key}` / `{key, value}` / `{key}` / `{prefix}` |
| `http.fetch` | `{method, url, headers?, body?}` → `{status, headers, body}`; declared hosts only, https only, never private addresses, no redirects, 15 s, 1 MB |
| `events.emit` | `{topic, payload}` → publishes `plugin.{name}.{topic}` (inside the document's transaction for hooks) |
| `sockets.send` / `sockets.close` | `{connection, message}` / `{connection, reason}` — on whichever replica holds the socket |
| `log` | `{level?, message}`: `info` (default), `warn` or `error`, to the server log with the plugin and tenant |
| `time.now` | → RFC 3339 UTC |

## Extension points and add-ons

An app can offer **extension points**, named places other apps plug into, and contribute to those
of the apps it requires. `app.yaml` declares both; they are checked when a bundle is read and
again by the installation plan:

```yaml
# Office offers a point...
id: office
extension_points:
  office.editor:
    description: Opens and edits files of given types
    schema: {type: object, required: [kind, types], properties: {kind: {enum: [wopi, bridge]}, types: {type: array, items: {type: string}}}}

# ...and an add-on contributes to it.
id: office-collabora
parent: office
requires: {office: "^1.0"}
contributes:
  office.editor:
    - {id: collabora, kind: wopi, types: [docx, xlsx, pptx]}
```

* A point is named `<owner app>.<name>`. Its `schema` describes what each contribution holds besides
  its `id`, in a JSON Schema subset: `type`, `description`, `properties`, `required`,
  `additionalProperties: false`, `items`, `enum`, `maxLength`, `maxItems`. Other keywords are
  refused rather than ignored. A contribution that breaks the schema fails the installation plan,
  which names the field.
* Contributions go to the screen slots below, to the app's own points, or to points of apps in its
  `requires`. Ids are unique within a point.
* The owner reads them with `GET /api/meta/contributions/{point}`. Only enabled apps count, and each
  item carries its `app` and `version`.
* An incompatible change to a point is a new major version of its owner, so `requires` ranges keep
  owners and add-ons in step.
* **Add-ons**: `parent` (also listed in `requires`) shows an app under the one it extends on the
  Apps page. Nesting needs no other rules: an add-on installs after its parent, and the parent
  cannot be disabled or removed while an add-on needs it.

## Records other apps share

The core holds only what every app needs: companies, currencies and their exchange rates, units.
People outside the workspace and their addresses are in `contacts`, customers and suppliers in
`parties`, items and price lists in `products`, projects in `project_records` (ERP's documents,
budgets and expense claims carry a project; the Projects app adds the work on it); ERP's packages (accounting, inventory, buying, selling,
manufacturing) bring the accounting, stock and buying records and add their fields to the shared
ones (a customer's receivable account, an item's stock settings). Depend on the least you need:
`parties` for customers, `products` for items, ERP's packages only for their own records.

* Every package must work with only its dependencies installed: the catalog refuses one whose
  links, overlays, workflows, seeds or personal-data rules reach further. What reaches further
  belongs in a connector, an app that requires both sides and ships with one of them: CRM's
  `crm-erp` (`requires: {crm, erp}`, package `crm_erp`) adds ERP's taxes to quotations and makes
  sales orders from them; its hooks register under `HookRegistry::scoped(&["crm_erp"], …)` so they
  run only where it is installed.
* A suite is a metadata app (`runtime: meta`, category `suite`) with only the apps it requires:
  installing it installs them. Connectors carry the category `connector`. The platform's suites
  are Workspace (Comms, Drive, Docs), Business (ERP, CRM, Helpdesk, Projects and their connectors)
  and Retail (ERP, Commerce, POS); a server lists those whose members it has.
* `ferrum task deps_report` lists what ties packages together: every entity a package names that
  another defines, and document hooks that run on another package's records or in every workspace.
* A field a package takes over from another's entity keeps its name, column and place in the form:
  `{op: add_field, column: true, after: <field>, field: {...}}` (packages only; no prefix). The
  entity's form may list it, and leaves it out where the package is not installed; one the form
  does not list goes into the section of the field it follows.

## Screens

Wasm and meta apps can bring screens: HTML pages under `ui/` in the bundle, with their scripts,
styles, WebAssembly, fonts and images. They plug into four slots:

| Slot | Shows | Item |
|---|---|---|
| `ui.page` | a page with a sidebar entry, `/app/x/<app>/<id>` | `title`, `entry`, `icon` |
| `ui.widget` | a card on Home | `title`, `entry`, `size: [w, h]` (12-column grid, 80 px rows) |
| `ui.panel` | a tab on the forms of `entity` | `title`, `entry`, `entity` |
| `ui.settings` | the app's settings screen, for administrators, from the Apps page | `entry` |

A screen runs in a sandboxed frame with an opaque origin, so it cannot read the web app's page,
storage or session.
* Its Content Security Policy lets it load only its own files: no other network and no form posts.
* A page opened outside a frame is refused.
* It talks to Ferrum through `ferrum.js` (ferrum-sdk `examples/board/ui/ferrum.js`). The host hands
  it a `MessagePort` and answers `context`, `api`, `navigate`, `openRecord`, `toast`, `confirm`
  and `resize`; it sends `theme` and `record` events.

`api` reaches `/api/v1/…`, `/api/meta/…` and the app's own `/api/ext/<app>/…`, and nothing else.
* The host marks these calls with `X-Ferrum-App`.
* The server runs them with the user's rights narrowed to what the app declared: its plugin's
  permissions and its own entities. They never run with administrator power. A screen can do
  neither more than its app was approved for nor more than its user can.
* A panel receives only its record's entity and id and reads the record through `api`, so it sees
  only the fields its app may read.

`examples/board` in ferrum-sdk is a complete example: a page, a widget, a panel, a settings screen
and an extension point.

## Interfaces of their own

An app may own a whole interface at addresses of its own, as `/deals/…`: a web app of any kind
(React, Vue, plain HTML) that the platform serves and wraps in the shell. Screens (above) stay the
way to add a page, card or panel to someone else's interface. The plan is
[docs/plan/apps-ui.md](plan/apps-ui.md); two small working examples are in
[docs/plan/spikes/u0](plan/spikes/u0).

```yaml
platform: ">=0.3.2, <0.4"
ui:
  dist: ui/dist                   # the built files, with an index.html
  shell: ">=1.0"                  # the shell API it needs
  family: Sales                   # optional: the family it heads (its add-ons with interfaces join it)
  csp:                            # origins beyond the workspace's host, reviewed by the store
    connect: ["setting:meet_url", "wss://media.example.com"]
contributes:
  shell.app:                      # launcher entries; each owns a route
    - {id: deals, route: deals, title: Deals, color: "#0f766e", roles: [Sales User], aliases: [pipeline]}
  shell.open:                     # which screens show a record and the list (/open/{entity}[/{id}])
    - {id: deal, entity: Deal, path: "deals/{id}", list: deals}
  shell.create:                   # the bar's Create menu
    - {id: deal, label: Deal, path: deals/new}
  shell.intent:                   # what it does for other apps
    - {id: pick, action: pick, type: note, path: notes/pick}
  shell.sidepanel:                # panels beside any app's page; `contextual` ones open from apps only
    - {id: deals, title: Deals, path: deals/_panel}
    - {id: deal, title: Deal, path: deals/_panel/deal, contextual: true}
  shell.public:                   # pages anyone with a link opens, without signing in
    - {id: quote, path: deals/q, title: Quote}
  shell.portal:                   # pages of the workspace's portal, at /portal/{id}/
    - {id: quotes, title: Your quotes, description: Quotes waiting for you, entity: Deal, open: "q/{id}"}
```

* **Routes** are `[a-z][a-z0-9-]{1,30}`, unique on a cluster with their aliases, and never one of the
  names the core keeps (`api ws files mcp metrics oauth open data admin store account portal
  accounts login logout s app home`, and anything starting with `_` or `.`). A release that takes
  another app's route is refused when it is made available.
* **`index.html`** has one `<head>` and one `<body>`; scripts are module files of the release
  (no inline scripts, nothing from elsewhere), built with a relative base (Vite: `base: './'`);
  no `<base>` and no Content-Security-Policy of its own. The release's files are named by the
  SHA-256 of their contents and served from `/_ui/{app}/{digest}/…`, cached for good.
* **Each page** gets, before the app's own scripts: the bootstrap (`#ferrum-bootstrap`: the
  workspace, the person, the app's routes and release), the workspace's theme (CSS variables
  `--ferrum-*`) and the shell runtime, which draws `<ferrum-bar>` and installs `ferrum.shell`.
* **`ferrum.shell`**: `ready()` (call it once the app has drawn), `navigate(path | {entity, id})`,
  `title()`, `breadcrumbs()`, `intent({action, type, data})` and `canIntent()`, `companion({kind,
  url})` for windows that outlive page loads (calls), `leaveGuard(isDirty)`, `live.subscribe(topics,
  onChange)`, `toast()`, `session.signOut()`, and `supports(capability)`. A page opened for an
  intent reads `intent.request()` and answers with `intent.done(result)` or `intent.cancel()`.
* **Sessions**: the page calls the API with the browser's session cookie; nothing to store.
* **Access**: an entry with `roles` is for administrators and people with one of them; without,
  for every member. Nobody else sees it in the launcher or opens its address.
* **Trust**: an interface runs on the workspace's host with the person's session. Apps compiled
  into the server, bundles signed by a key the server trusts, and apps an operator makes available
  for the whole cluster may bring one; an unsigned app private to a workspace may not.
* **`ferrum.shell`, more**: `sidepanel.open({id, context})` and `close()` (a panel's page reads
  `sidepanel.context()` and hears `panel-context`); `uploads.start({file, url})` hands an upload to
  the runtime, which keeps the file in IndexedDB until it is sent, carries on in the next page of any
  app, shows it in the bar's tray and sends `Idempotency-Key` (the upload's id) so a resent upload is
  stored once where the endpoint honours it; `family()` gives the app's family and its entries (the
  kit's `FamilyNav` draws them); `apps()` the launcher's entries; `on(event, cb)` returns a function
  that stops listening.
* **Records-only apps**: an app without `ui` and without `shell.app` entries gets a launcher entry
  into its records anyway, at `/data/{app}/` (its manifest's `name`): lists, forms and
  reports of its packages' entities and its add-ons', in the frame ERP's interface uses, where
  `/open` of its entities leads. Connectors (category `connector`), suites and add-ons get none: an
  add-on's records are its parent's.
* **Families**: an app with an interface heads a family when it names one (`ui.family`) or when its
  add-ons (`parent`) bring interfaces too. They share its launcher group and navigation, and each
  still ships and versions on its own (Drive and Docs are Workspace).
* **Side panels** open beside the page from the bar (one at a time, resizable, again on the tab's
  next page) or, `contextual`, from an app with something to show (`sidepanel.open({id: "drive.details",
  context: {file}})`). On a phone the panel's app opens instead.
* **Public pages** (`shell.public`) are below one of the app's routes and take the workspace in the
  path: `/{path}/{workspace}/…` (`/drive/s/acme/{link}`). They have no bar and no person (the
  bootstrap's `person` is null, `host.kind` is `public`), send no referrer and are kept out of
  search engines; on a host that serves one workspace only that workspace's links work.
* **Portal pages** (`shell.portal`, [plan](plan/apps-ui.md) §9.1) are drawn by the app's own
  interface at `/portal/{id}/` for portal people (customers, suppliers) and members alike: the page
  reads `app.entry` (`portal/{id}`) from the bootstrap to know it is there (its router's base is
  `routes["portal/{id}"]`), gets `<ferrum-portal-bar>` instead of the bar and `host.kind:
  "portal"`, and calls the app's own endpoints (`/api/app/{package}/…`). Portal people hold no
  roles, so those endpoints find what is theirs by `ctx.email` (proven by the emailed code they
  signed in with) and read or change it as the system after that check, rate limited. `entity` and
  `open` (plain segments with `{id}`) send portal people's `/open/{entity}/{id}` to `/portal/{id}/{open}`.
  The first app to give an id keeps it.
* **Public sites**: on a host whose target is `site:{app}` (§7.3), every page is the app's: the
  server calls its package's site handler with `GET /_site/{path}` and the headers
  `x-ferrum-site-api` (`/api/site/{workspace}/{package}`, for the page's scripts) and
  `x-ferrum-site-origin` (`https://shop.example.com`, for canonical links and sitemaps), which only
  the server sets. The handler answers HTML, CSS, scripts, `robots.txt` or `sitemap.xml` as it likes;
  the server adds `X-Content-Type-Options`, `Referrer-Policy` and a strict Content-Security-Policy
  unless it sets its own. Commerce's storefront is the example.
* **CSP**: kinds are `connect`, `frame`, `img`, `media`, `font`, `worker` and `form` (where forms may
  post); `https:` alone is allowed for `img` and `media`; apps compiled into the server may name a
  server setting, `env:FERRUM_…`, whose addresses (several, space- or comma-separated) are read at
  run time, and a `wss://` one adds its `https://` twin for `connect`.
* **Building one** (first-party apps, until `@ferrum/*` are on npm): the app's `ui/` builds with
  Ferrum-Frontend's packages from the sibling checkout, `node ../../ferrum/web/packages/build/cli.mjs
  build|check|typecheck|test`, with `vite.config.mjs` = `ferrumApp(import.meta.url)`. Its code imports
  `@ferrum/*` and libraries by name and never Ferrum-Frontend's own modules. `dist/` is committed and
  embedded in the app's crate (`UI`); `check` (in CI) fails when `dist/` was not built from the sources
  as they are. Comms, Drive and Docs are built this way.
* **Design**: the shell owns only the bar; the kit (`@ferrum/kit`, shadcn components on the theme's
  tokens) is what Ferrum's own apps use, and any app may bring its own look and framework instead.
* An app without packages may consist of an interface alone.

## Limits and guarantees

* No ambient access: no WASI, no files, no network except declared hosts, no clock except `time.now`.
* Fuel (deterministic CPU), memory cap, 1 000 host calls and 1 MB of output per call; nested
  plugin work stops after 4 levels.
* Failures are fail-closed and logged; disabling a plugin takes effect within seconds on every replica.
* Stateless: plugin state lives in PostgreSQL (`kv`, its entities). Events are delivered at least
  once and recorded per plugin, schedules fire on one replica at a time, socket messages reach the
  replica holding the connection (cross-replica messages up to 7 KB). WebSockets are rate limited
  (100 messages per 10 s), text only, 64 KB per message, with keepalive pings.

## EV charging, for example

An OCPP 1.6J plugin declares `sockets: [{path: "/ocpp/*", public: true, subprotocols: ["ocpp1.6"]}]`
and entities such as `XChargePoint`, `XConnector` and `XChargingSession`. `BootNotification`,
`Heartbeat`, `StatusNotification`, `StartTransaction`, `MeterValues` and `StopTransaction` arrive
as socket messages and become records; operators' "start charging" and "unlock" buttons are
routes that `sockets.send` a `RemoteStartTransaction` or `UnlockConnector` to the charge point.
A schedule marks silent charge points offline, an event subscription turns finished sessions into
sales invoices, and OCPI roaming is HTTPS to declared partner hosts plus public routes for their
callbacks.
