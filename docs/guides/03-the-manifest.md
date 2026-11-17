# The manifest

`app.yaml` at the root of an app says what the app is, what it needs and what it brings. The server, the
`ferrum-app` command and the store all read it with the same parser, `AppManifest::parse` in
`crates/apps/src/manifest.rs`, so what passes one passes the others (`crates/apps/src/lib.rs`). This guide goes
through every field that parser knows, with the rule each check holds it to and the refusal you see when it does
not.

The tests in `crates/apps/src/manifest.rs` (`mod tests`) and `crates/apps/src/shell.rs` show every refusal below
with the input that causes it. When this guide and those tests differ, the tests are right.

## How the parser reads

- **Unknown fields are refused, never skipped.** Every struct is `deny_unknown_fields`. A field the server does
  not know may be a requirement of a newer release, so the error says so:
  `app.yaml: unknown field `services` …. This server does not know that field, and will not ignore what may be a
  requirement: this app needs a newer Ferrum`. The same holds inside `resources`, `operator_settings`, `settings`,
  `access`, `deprecation`, `industrial`, `ui` and each extension point.
- **Required**: `id`, `name`, `version`, `publisher`, `summary`, `runtime`, `platform`, and `categories` (one to
  three). Everything else has a default.
- **Every error starts with `app.yaml:`** (`AppError::Manifest`), then names the field: `settings.mode: a select
  setting needs options`.
- **Two checks run later**, when there is more than the manifest to look at: the bundle checks that files and
  packages exist and that a third party's entities carry its prefix (`crates/apps/src/bundle.rs`); the
  installation plan checks contributions to another app's extension point against that point's schema
  ([docs/plugins.md](../plugins.md), *Extension points and add-ons*).

`scripts/check_apps.py` asks more of the apps the platform compiles in than the parser does: `license`,
`homepage`, `support` and `description` are optional to the parser and required by the contract
([docs/app-contract.md](../app-contract.md)).

## A complete example

An app with an interface of its own, after GRC's `app.yaml` (`ferrum-app-grc`) and Cloud's `operator_settings`
(`ferrum-app-cloud/app.yaml`):

```yaml
id: visits                         # [a-z][a-z0-9-]{1,39}
name: Site Visits                  # one line, at most 60 characters
version: 0.4.1                     # semver, no build metadata
publisher: cbferrum                # same syntax as an id
summary: "Visits to customers' sites, planned and done, with what was found"   # one line, at most 200
description: README.md             # a markdown file in the bundle
categories: [projects]             # one to three of CATEGORIES
icon: assets/icon.svg              # optional: an image under assets/
screenshots: [assets/list.png]     # optional: at most 10, each once
license: AGPL-3.0-only
homepage: https://github.com/CBFerrum/ferrum-app-visits
support: https://github.com/CBFerrum/ferrum-app-visits/issues   # or an e-mail address
runtime: native                    # native | wasm | meta | service
platform: ">=0.3.9, <0.4"          # the app API versions it runs on
requires:                          # other apps: id → semver range
  erp: ">=0.21, <0.22"
packages: [visits]                 # directories under packages/
uninstall: ask                     # keep (default) | ask
settings:                          # the app's settings page, which each workspace's administrators fill
  - {key: reminder_days, label: Remind visitors (days before), type: number}
operator_settings:                 # what the installation's operator chooses (native only)
  - {key: maps_url, env: FERRUM_VISITS_MAPS_URL, doc: "The map tiles server browsers reach."}
ui:
  dist: ui/dist
contributes:
  shell.app:
    - {id: visits, route: visits, title: Site Visits, icon: calendar-check, roles: [Visit Planner]}
  shell.open:
    - {id: visits, package: visits, path: "visits/e/{entity}/{id}", list: "visits/e/{entity}"}
```

## Identity and the store

| Field | Rule | Refused with |
|---|---|---|
| `id` | 2 to 40 characters: a lower-case letter, then lower-case letters, digits or `-`. Not `ui` or `shell` (the platform's points). Unique in the store | `id 'EvCharging' must be 2-40 characters: …`; `the id 'ui' is reserved for the platform's points` |
| `name` | required, one line, at most 60 characters | `name is required`; `name is longer than 60 characters`; `name must be a single line of text` |
| `version` | semver; a pre-release (`1.4.0-beta.2`) is allowed, build metadata (`+build.7`) is not | `version 1.4.0+build.7 must not carry build metadata (+build.7)`; `1.4` does not parse |
| `publisher` | the store's publisher id, same syntax as `id` | `publisher 'Volt Works' must be a store publisher id: …` |
| `summary` | required, one line, at most 200 characters | `summary must be a single line of text` |
| `description` | a path in the bundle ending `.md` | `description must name a markdown file in the bundle (such as README.md), not notes.txt` |
| `categories` | one to three, each once, from `CATEGORIES` | `categories: list 1 to 3 of: …`; `unknown category 'games'`; `category 'energy' is listed twice` |
| `icon` | an image under `assets/`: png, jpg, jpeg, gif, webp or svg | `icon: icon.svg must be an image under assets/ …` |
| `screenshots` | images under `assets/`, at most 10, none twice | `at most 10 screenshots`; `a screenshot is listed twice` |
| `license` | one line, at most 100 characters | `license is required` (when present and empty) |
| `homepage` | an `http://` or `https://` URL of at most 300 characters, no quotes, spaces or angle brackets | `homepage must be an http(s) URL, not 'javascript:alert(1)'` |
| `support` | such a URL, or an e-mail address | `support must be an http(s) URL or an e-mail address, not 'nobody'` |
| `deprecation` | `{support_ends: YYYY-MM-DD, replacement?: <app id>, export: <how to get the data out>}` | `deprecation.support_ends must be YYYY-MM-DD, not soon`; a deprecation without `export` |
| `uninstall` | `keep` (the default): removing the app keeps its data; `ask`: removing offers to delete the workspace's rows | `unknown variant` |

The categories (`CATEGORIES`): `sales`, `crm`, `support`, `hr`, `finance`, `inventory`, `manufacturing`,
`projects`, `commerce`, `pos`, `communication`, `productivity`, `marketing`, `analytics`, `integrations`,
`localization`, `energy`, `iot`, `education`, `healthcare`, `industry`, `developer`, and `suite` and `connector`
for apps that bundle or join others ([01-what-an-app-is.md](01-what-an-app-is.md)).

**The description** is shown outside GitHub: the store's page renders it as HTML and the install dialog as a plain
excerpt (`crates/apps/src/description.rs`). Both keep the Markdown and drop the rest: raw HTML never passes,
images are dropped (the store shows `screenshots`), Mermaid and other diagram blocks go, and links are http(s),
mailto or the description's own headings. A relative link resolves against the GitHub repository `homepage` names,
and is otherwise dropped with its text kept. Write the README so that it still says what the app is with the
pictures and HTML taken out.

**What the platform's check adds** (`scripts/check_apps.py`, for the apps the lock compiles in): `id`, `name`,
`version`, `publisher`, `summary`, `categories`, `license`, `homepage` and `support` all present
(`app.yaml has no homepage`); the licence one of `AGPL-3.0`, `GPL-3.0`, `LGPL-3.0`, `MIT`, `Apache-2.0`,
`BSD-2-Clause`, `BSD-3-Clause`, `MPL-2.0`, matched by its start (`AGPL-3.0-only` passes); the description file
there; `CHANGELOG.md` with a heading naming the version. An add-on takes `license`, `homepage`, `support` and
`description` from its parent's manifest when it has none.

## Runtime and code

| Field | Rule | Refused with |
|---|---|---|
| `runtime` | `native`, `wasm`, `meta` or `service` | `unknown variant` |
| `plugin` | `wasm` apps only, and exactly `plugin.json` (the module is `plugin.wasm`) | `wasm apps must set plugin: plugin.json`; `only wasm apps have a plugin (this app's runtime is native)` |
| `service` | `service` apps only: `{url, events}` | `service apps must say where the service is (service.url)`; `only service apps have a service section` |
| `service.url` | http(s), at most 200 characters, no query, fragment or user | `service.url must be an http(s) URL of at most 200 characters, without a query, fragment or user` |
| `service.events` | at most 50 topics like `SalesInvoice.submitted`, `SalesInvoice.*` or `*`, each once, each about an entity `access` reads | `service.events: 'Employee.created' is about Employee, which its access does not read` |
| `prefix` | 2 to 8 letters or digits, starting with a capital, not with `X` (custom and plugin entities) | `prefix 'ev' must be PascalCase …`; `prefix 'Xv' cannot start with X …` |

**The prefix** is a third party's namespace, reserved per publisher in the store. In wasm and meta apps, package
entities are named `<prefix><Capital or digit>…` (`EvChargePoint`, not `Event`), plugin entities `X<prefix>…`,
fields added to other packages' entities start with `<snake prefix>_`, and package roles may not grant `*`
(`crates/apps/src/lib.rs`, *Third-party namespaces*). Native apps are reviewed platform code and carry none.

A `meta` app needs at least one package, an interface of its own, or the apps of its suite (`a meta app needs at
least one package, an interface of its own (ui), or the apps of its suite (requires)`).

## Versions, `platform` and `requires`

**`version`** is the app's own semver. A release that changes behaviour people rely on, or the shape of an
extension point it offers, is a new major version (for `0.x`, a new minor): `requires` ranges keep owners and
add-ons in step ([docs/plugins.md](../plugins.md)). How versions, the CHANGELOG and pinning go together is
[16-releasing.md](16-releasing.md).

**`platform`** is a semver range of the *app API* the app works with, checked against `PLATFORM_VERSION` in
`crates/apps/src/lib.rs` (0.3.9). The app API's version moves when the platform adds something an app
may depend on; the comments in the code and docs name what came when:

| App API | What an app may use from it | Where it is said |
|---|---|---|
| 0.3.2 | an interface of its own (`ui`, the shell's points) | [docs/plugins.md](../plugins.md), *Interfaces of their own* |
| 0.3.8 | `shell.menu`: a row of a module's left menu leading to the app's own page | [docs/plugins.md](../plugins.md) |
| 0.3.9 | its vault handle (`HookRegistry::vault`) and `operator_settings` | `PLATFORM_VERSION` in `crates/apps/src/lib.rs`; [docs/app-contract.md](../app-contract.md) |

Write the range with an upper bound below the next minor: `">=0.3.9, <0.4"`. The lower bound is the oldest app API
that has everything the app uses. `scripts/check_apps.py` fails an app that declares `operator_settings` with a
`platform` whose lower bound is below 0.3.9 (`operator_settings need the platform that reads them: platform:
">=0.3.9, <0.4"`). The App Manager refuses to install an app whose range the server does not meet: the plan lists it
as `{"app": "platform", "need": …, "have": "0.3.9", "action": "upgrade"}` (`blocked` in `server/src/appmgr.rs`).

**`requires`** maps other apps' ids to semver ranges. An app cannot require itself (`an app cannot require
itself`), and each key is an app id. Installing resolves the ranges: missing apps are installed first, an
installed app is never changed unless asked, never downgraded, and older versions are tried when the newest
conflict (`resolve` in `crates/apps/src/resolve.rs`). An app another installed app requires cannot be uninstalled
or disabled first.

### How ranges go wrong

| Mistake | What happens | What catches it |
|---|---|---|
| A range that stops below a sibling's new release (`comms: ">=0.7, <0.8"` while Comms moves to 0.10) | The app cannot be installed beside the release the platform compiles in: its requirement is never met | `compiled_apps_fit_together` in `server/src/packages.rs` fails: `campaigns requires comms >=0.7, <0.8, compiled with comms 0.10.0` |
| No upper bound (`erp: ">=0.21"`) | A breaking release of the other app is accepted, and the app breaks at run time | reviewed, not checked |
| A lower bound older than what the code uses | An older release of the other app is accepted, and a call or field the app needs is missing | reviewed, not checked; the scenario test runs against the version the platform pins, not the oldest in range |
| `^0.3` read as "any 0.x" | For `0.x`, `^0.3` means `>=0.3.0, <0.4.0` (semver's caret on a zero major) | — |
| A `platform` range that leaves out the platform being built | The app is refused at install (`blocked`) | `compatible_with` in the App Manager |

The fix for the first is always in the app that falls behind: raise its range in the same pull request that moves
the sibling, release it, and pin both together ([16-releasing.md](16-releasing.md)).

## Records and access

| Field | Rule | Refused with |
|---|---|---|
| `packages` | directory names under `packages/`, each a package name (`[a-z][a-z0-9_-]{0,62}`), each once | `packages: 'Ev' is not a package name …`; `packages: 'a' is listed twice` |
| `parent` | the app this one is an add-on of; it must also be in `requires` | `parent 'crm' must also be listed in requires` |
| `access` | at most 100 entries `{entity, read, write, create, delete, submit, cancel, export, import, if_owner, read_groups, write_groups}`; `entity` an entity name or `*` | `access: 'customer' is not an entity name (or *)` |
| `resources` | what the app needs beyond the platform's records (below) | see below |

**`access`** is what an app's pages may reach for the person, within the person's own rights, when the app has no
plugin declaring permissions: the records of other apps an interface reads. It is consented to at install with the
rest of the app's permissions, and every call from its pages is narrowed to it, never run as an administrator
([docs/plugins.md](../plugins.md), *`access`*; `AccessDef::permission` turns each entry into the platform's
permission rule). A `service` app's account holds exactly `access`.

**`resources`** (`ResourceDef`): each `{name, class, version, required, semantics, isolation, authority,
classification, retention_days, backup, on_uninstall, capacity: {storage_gb, connections}}`. It says what is
needed; who provides it is the workspace's choice, and any change is something someone agrees to again
(`consent_line`).

| Key | Values |
|---|---|
| `name` | `[a-z][a-z0-9_]{0,39}`, each once |
| `class` | `relational`, `object`, `time_series`, `search`, `cache`, `stream` |
| `version` | the class contract's version, from 1 |
| `isolation` | `database`, `prefix` |
| `authority` | `primary`, `derived`, `archival`, `disposable` |
| `semantics` | any of `transactions`, `row_security`, `read_after_write`, `conditional_writes`, `immutable_versions`, `point_in_time_recovery` |
| `classification` | `public`, `internal`, `confidential`, `restricted` |
| `on_uninstall` | `retain`, `offer_deletion` |

A `meta` app has no code to use one (`resources: an app with no code has nothing to use them with`), nor a
`service` app (`… where the platform's resources do not reach`). An unknown value is refused, never ignored
(`resources.ledger.semantics: this server does not know 'teleportation', and will not ignore what may be a
requirement`).

## Settings

Two kinds, for two people, never confused ([07-stateless-apps.md](07-stateless-apps.md) has why):

**`settings`**: the app's settings page, which each workspace's administrators fill (`SettingDef`).

| Key | Rule |
|---|---|
| `key` | snake_case, each once |
| `label` | one line, at most 100 characters |
| `type` | `text`, `secret` (write-only), `number`, `check`, `select` |
| `options` | a `select`'s choices, each once; only for `select` |
| `required` | `true` or `false` |
| `description` | one line, at most 500 characters |

Refusals: `settings: key 'OcpiToken' must be snake_case …`, `settings.ocpi_token: type must be one of …, not
'password'`, `settings.mode: a select setting needs options`, `settings.x: options are only for select settings`.

**`operator_settings`**: what the installation's operator chooses and the app's code in the server reads
(`OperatorSetting`; [docs/app-contract.md](../app-contract.md), *Operator settings*). Native apps only.

| Key | Rule |
|---|---|
| `key` | snake_case; `<app id>.<key>` at most 63 characters (it is the `setting` table's key) |
| `env` | the environment variable that gives the setting too, and wins over the database's value: an upper-case letter, then upper-case letters, digits and `_`, 3 to 100 characters, never `DATABASE_URL`; each once |
| `type` | `text` (default), `bool`, `int`, `json`, `choice` |
| `values` | a `choice`'s values, each once (`""` among them when it may be empty); only for `choice` |
| `default` | as the variable would say it, and a value of the type; `""` for the app's own default |
| `applies` | `start` (default): at a replica's next start; `live`: every replica within seconds |
| `sends` | for an address given an environment's secret: that secret's variable; not one of the app's own `env` |
| `doc` | what it does, for the operations guide; 1 to 1000 characters |

There is no field for a secret, and a `secret: true` is an unknown field. Read each with
`ferrum_kernel::env::var(env)`, never `std::env::var`: `scripts/check_apps.py` fails `src/x.rs:12 reads
FERRUM_VISITS_MAPS_URL with std::env::var: read it with ferrum_kernel::env::var, or the database's value never
reaches it`. Two owners of one key or variable stop the server; `scripts/settings.py` checks the same across the
platform's registry and every compiled app.

## Extension points and contributions

**`extension_points`**: points other apps may contribute to, each named `<this app's id>.<name>`, with a
`description` (one line, at most 300) and an optional `schema` in the JSON Schema subset `crates/apps/src/contract.rs`
checks (`type`, `description`, `properties`, `required`, `additionalProperties: false`, `items`, `enum`,
`maxLength`, `maxItems`; any other keyword is refused). At most 20.

**`contributes`**: point → items, each with an `id` (lower-case letters, digits, `_`) unique within the point, 1 to
50 items a point, contributions to at most 20 points. A point is one of: a shell point (below), one of the app's own points, or a point of an app in `requires`.
Anything else: `contributes: 'crm.quote_source' is not a shell point (…), an extension point of this app, or one of
an app in requires`.

### The `ui` section and the shell's points

```yaml
ui:
  dist: ui/dist          # a directory of built files with an index.html; not under packages/ or assets/
  family: Sales          # optional: the family it heads; an add-on names none
  csp:                   # origins beyond the workspace's host, by kind, at most 10 a kind
    connect: ["setting:meet_url", "wss://media.example.com"]
```

`csp` kinds are `connect`, `frame`, `img`, `media`, `font`, `worker`, `form`. An origin is `https://…` or
`wss://…` (one `*.` wildcard at most), `setting:<key>` for one of the app's `settings`, `https:` alone for `img` and
`media`, or, for native apps, `env:FERRUM_…` for a server setting
(`ui.csp.connect: only apps compiled into the server name server settings (env:FERRUM_X)`).

The shell's points (`SHELL_POINTS` in `crates/apps/src/shell.rs`); compiled-in apps contribute to these:

| Point | Item (required in bold) | What it does |
|---|---|---|
| `shell.app` | **`title`**, `route` or `entity`, `description`, `icon`, `color` (`#rrggbb`), `roles`, `aliases`, `compact`, `priority`, `installable` | a launcher entry; with `route`, it owns `/<route>/` and needs `ui`; with `entity`, a records-only entry |
| `shell.open` | **`path`**, `entity` or `package`, `list`, `priority` | which page shows a record (`/open/{entity}/{id}`) and the list; a `package` entry's path holds `{entity}` and `{id}` |
| `shell.create` | **`label`**, **`path`**, `entity` | the bar's Create menu |
| `shell.intent` | **`action`**, **`type`**, **`path`**, `title` | what the app does for other apps (`pick`, `create`, `compose`, `share`, `preview`, `schedule`, or its own) |
| `shell.sidepanel` | **`title`**, **`path`**, `icon`, `contextual` | a panel beside any app's page |
| `shell.search` | **`endpoint`**, `title` | a search provider: `/api/app/{app}/…` or `/api/ext/{app}/…` |
| `shell.notification` | **`label`**, `default` (of `inapp`, `email`, `digest`, `push`) | a kind of notification people may turn on or off |
| `shell.work` | **`endpoint`**, `title` | items for Home's My work |
| `shell.card` | **`entity`**, **`endpoint`** | a record's card |
| `shell.public` | **`path`**, `title` | a page anyone with a link opens, below a route (`drive/s`), never at it |
| `shell.portal` | **`title`**, `description`, `entity` with `open` | a page of the workspace's portal at `/portal/{id}/` |
| `shell.assistant` | **`title`**, **`instructions`** (≤ 2000), `suggestions` (≤ 6) | what the assistant should know in the app's pages ([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md)) |
| `shell.menu` | **`module`**, **`path`**, **`label`**, `icon`, `roles`, `after` | a row of a module's left menu for the app's own page |

Rules the check holds them to, each with its refusal in `crates/apps/src/shell.rs`:

- **Routes** are 2 to 31 characters, `[a-z][a-z0-9-]`, unique with their aliases, and not one the core keeps:
  `api`, `assistant`, `ws`, `files`, `mcp`, `metrics`, `oauth`, `open`, `data`, `admin`, `store`, `account`,
  `portal`, `public`, `accounts`, `login`, `logout`, `s`, `app`, `home`, `sw.js`, `robots.txt`, `favicon.ico`,
  `manifest.webmanifest`, nor anything starting with `_` or `.` (`'admin' is kept by the core`).
- **Paths** start with one of the app's routes and hold only plain segments and the point's placeholders
  (`'deals/../x' may hold only plain path segments`); a `shell.open` path must place `{id}`.
- **A `shell.app` entry** gives a route or an entity, not both; an icon is a name (lower-case, digits, `-`) or an
  image under `assets/`; `aliases` only with a route.
- **One handler per intent**, **one page per entity** (`contributes.shell.open: Deal is opened twice`), and no two
  public pages that overlap.
- **The icon must be one the shell draws**: `scripts/check_apps.py` reads `const ICONS` in `server/shell/runtime.js`;
  `compiled_apps_fit_together` asks `crate::shell::lucide(icon)`.
- **An app's routes are its own on the cluster**: `compiled_apps_fit_together` fails `/r/ belongs to both a and b`.

An isolated interface (an unsigned app uploaded to one workspace) may not contribute `shell.public`,
`shell.sidepanel`, `shell.intent` or `shell.portal`, nor name other origins in `csp`
([docs/plugins.md](../plugins.md), *Isolated interfaces*).

## Industrial solutions

`industrial` (`IndustrialSection` in `crates/apps/src/industrial.rs`): the site runtime's connectors an app needs
and the equipment files it ships.

```yaml
industrial:
  connectors:
    - {scheme: modbus+tcp, version: ">=1.0"}   # version defaults to *
  device_profiles: [industrial/profiles/meter.yaml]
  acceptance: [industrial/acceptance/commissioning.yaml]
```

The bundle is refused when a named file is missing or malformed, when a profile speaks a scheme the section does not
declare, or when an acceptance step names a profile, point or command the bundle does not define. Which schemes a
site can open is the site runtime's to say; this checks names only. See
[docs/industrial-solutions.md](../industrial-solutions.md).

## Mistakes, and what catches them

| Mistake | Refusal | From |
|---|---|---|
| A field the server does not know | `unknown field …: this app needs a newer Ferrum` | parser |
| `parent` not in `requires` | `parent 'x' must also be listed in requires` | `validate_extensions` |
| `route` without `ui` | `a route needs the app's own interface (a ui section)` | `shell::check` |
| `operator_settings` on a wasm app | `operator_settings: only an app compiled into the server reads them` | parser and `check_apps.py` |
| `operator_settings` with `platform: ">=0.3, <0.4"` | `operator_settings need the platform that reads them` | `check_apps.py` |
| A declared variable read with `std::env::var` | `… reads FERRUM_X with std::env::var …` | `check_apps.py` |
| A launcher icon the shell lacks | `the shell draws no x icon` | `check_apps.py`, `compiled_apps_fit_together` |
| A version with no CHANGELOG heading | `CHANGELOG.md says nothing of 0.4.1` | `check_apps.py` |
| A range left behind a sibling | `x requires y …, compiled with y …` | `compiled_apps_fit_together` |
| A contribution breaking its point's schema | `contributes.office.editor.collabora: …` (names the field) | parser for the app's own points; the installation plan and `compiled_apps_fit_together` for another app's |
| Two apps' reports with one name | `both a and b give the report x` | `compiled_apps_fit_together` |

## Checked by

- `crates/apps/src/manifest.rs` (`AppManifest::parse`, `validate`) and its tests; `crates/apps/src/shell.rs`
  (`check`) and its tests; `crates/apps/src/industrial.rs`; `crates/apps/src/contract.rs`.
- `crates/apps/src/bundle.rs`: paths, files, the prefix of a third party's entities.
- `scripts/check_apps.py`: the contract's fields, the licence, the description, the history, the icon,
  `operator_settings` and how they are read.
- `scripts/settings.py`: one owner per setting across the platform and the compiled apps.
- `server/src/packages.rs`, `compiled_apps_fit_together`: ranges between compiled apps, contributions, icons,
  routes, report names.
- `server/src/appmgr.rs` (`blocked`, `compatible_with`): `platform` and `requires` at install.
- Reviewed, not checked: upper bounds on ranges, a lower bound that matches what the code uses, a major (or `0.x`
  minor) for a breaking change.

## See also

- [02-your-first-app.md](02-your-first-app.md): the manifest in a working app.
- [07-stateless-apps.md](07-stateless-apps.md): operator settings and a workspace's settings.
- [16-releasing.md](16-releasing.md): versions, the CHANGELOG, pinning.
- [17-the-interface.md](17-the-interface.md): what the `ui` section ships.
- [docs/plugins.md](../plugins.md), [docs/plan/apps.md](../plan/apps.md), [docs/plan/apps-ui.md](../plan/apps-ui.md),
  [docs/app-contract.md](../app-contract.md).
