# What an app is

An app is the unit a workspace installs: a manifest (`app.yaml`), the records it brings (packages), and,
depending on its kind, code and an interface. Everything beyond the platform is one: ERP, CRM, Mail, Drive,
a country's tax rules, an EV charging integration. This guide says what each kind of app is for, where its code
lives, and what belongs in the platform instead.

The normative text is [docs/app-contract.md](../app-contract.md) (what every app gives) and
[docs/plan/apps.md](../plan/apps.md) (the format, the store and the runtimes). This guide is how to read them.

## What an app is, and is not

An app is:

- **Installed per workspace.** A release is made *available* on a cluster once (its packages join the catalog
  and the kernel creates their tables), then each workspace *installs* it: its packages become part of that
  workspace's schema, its roles and menus appear, its hooks start running there
  ([docs/plan/apps.md](../plan/apps.md) §2 and §6).
- **Versioned on its own.** It has its own semver `version`, its own `CHANGELOG.md` and its own repository. It
  says which platform it runs on (`platform`) and which other apps it needs (`requires`); see
  [03-the-manifest.md](03-the-manifest.md).
- **Disabled and removed without touching Ferrum.** Disabling stops its hooks, routes, schedules and menus and
  keeps its data; removing it takes its packages out of the workspace, and deletes the workspace's rows only when
  the manifest says `uninstall: ask` and the administrator agrees.

An app is not:

- **A fork of the platform.** An app never edits a platform crate. What it needs from the platform it asks for
  in the platform's own repository, as a change to the platform (see *What belongs in the platform*, below).
- **A place for a country or a sector inside an engine.** Engines are general. A country's rules or a sector's
  are data or an add-on (*Engines and content*, below).
- **A store of its own state.** An app keeps nothing on a local disk, no secret in a setting or the environment,
  no file outside the platform's file store: [07-stateless-apps.md](07-stateless-apps.md),
  [08-secrets-and-the-vault.md](08-secrets-and-the-vault.md), [09-files-and-storage.md](09-files-and-storage.md).

## The kinds

The manifest's `runtime` decides what an app can do (`crates/apps/src/manifest.rs`, `enum Runtime`):

| Runtime | Where its code runs | What it reaches | Used for |
|---|---|---|---|
| `native` | compiled into the server | everything the kernel offers: tables of its own, hooks in the document's transaction, SQL, sockets, endpoints, upkeep | Ferrum's own apps (ERP, CRM, Mail, Drive, the countries) and self-hosters' builds; reviewed code |
| `wasm` | a WebAssembly sandbox with fuel and memory limits | only what `plugin.json` declares, consented to at install | third-party integrations, gateways, verticals |
| `meta` | nowhere: metadata only | its packages' records, and its pages' records within `access` | industry kits, templates, suites |
| `service` | its publisher's servers, over HTTPS | as an account of its own, the records `access` names, over the API | code a publisher must run itself |

The table is the one in [docs/plugins.md](../plugins.md), *Services on their own servers*. Each kind has rules
the manifest parser enforces: only a `wasm` app has `plugin: plugin.json`, only a `service` app has a `service`
section, only a `native` app has `operator_settings`, a `meta` app has no `resources`
(`AppManifest::validate` in `crates/apps/src/manifest.rs`).

### A native app

A Rust crate compiled into the server. It exposes the same entry points every native app does, which the server's
`apps!` list in `server/src/packages.rs` calls:

| Item | What it is | In GRC (`ferrum-app-grc/src/lib.rs`) |
|---|---|---|
| `MANIFEST: &str` | its `app.yaml`, `include_str!` | `pub const MANIFEST: &str = include_str!("../app.yaml");` |
| `packages()` | its packages, embedded at build time by `build.rs` | `ferrum_meta::catalog::load_embedded(EMBEDDED)` |
| `register(&mut HookRegistry)` | its document hooks, event sinks, endpoints, jobs | five `DocHook`s, one per entity |
| `reports()` | its reports (`ReportSource`s) | none yet |
| `maintenance(&Kernel)` | its upkeep, run every minute by one replica at a time | expires acceptances, keeps controls' standing |
| `UI` | its built interface (`ui/dist`), when it has one | embedded by `build.rs` |

The server registers each as itself: `hooks.app(id, &packages, |h| krate::register(h))`, then
`hooks.own(id, &packages)` (`server/src/packages.rs`, `macro_rules! apps`). That registration is what gives the
app its own vault handle ([08-secrets-and-the-vault.md](08-secrets-and-the-vault.md)) and scopes its document
hooks to the workspaces that installed one of its packages. A native app is trusted code: the platform's review
is what stands between it and other workspaces' data, which is why it lives in a repository the platform's
maintainers review and pin (`docs/plan/apps.md` §1, §11).

How the platform takes one in is [02-your-first-app.md](02-your-first-app.md), *Putting it in the platform*.

### An add-on

An app that extends another: the manifest names the other as `parent`, which must also be in `requires`
(`validate_extensions` refuses `parent 'x' must also be listed in requires`). The Apps page shows it under its
parent; it installs after its parent, and the parent cannot be disabled or removed while it is installed
([docs/plugins.md](../plugins.md), *Extension points and add-ons*).

Add-ons live in their parent's repository when they ship with it. ERP's are under
`ferrum-app-erp/addons/` (India, Germany, the EU, the warehouse, the shop floor and more), each with its own
`app.yaml`:

```yaml
# ferrum-app-erp/addons/india/app.yaml (abridged)
id: india
name: India
version: 0.7.3
publisher: cbferrum
summary: GST, e-invoices and e-way bills for ERP
categories: [finance]
runtime: native
platform: ">=0.3.8, <0.4"
requires: {erp: ">=0.21"}
parent: erp
packages: [india]
```

A crate that holds several add-ons keeps one manifest per add-on under `apps/<code>.yaml`
(`ferrum-app-erp/addons/countries/apps/sg.yaml`, `jp.yaml`, …); `scripts/check_apps.py` reads both forms. An
add-on may take `license`, `homepage`, `support` and `description` from its parent's manifest at its repository's
root (`INHERITED` in `scripts/check_apps.py`), and its history is its repository's `CHANGELOG.md`.

Two kinds of add-on have categories of their own (`CATEGORIES` in `crates/apps/src/manifest.rs`):

- **A connector** (`connector`) joins two apps that do not depend on each other: it requires both and ships with
  one. CRM's `crm-erp` (`requires: {crm, erp}`, package `crm_erp`) adds ERP's taxes to quotations; its hooks
  register under `HookRegistry::scoped(&["crm_erp".to_string()], …)` so they run only where it is installed
  ([docs/plugins.md](../plugins.md), *Records other apps share*). The connector manifests that ship with their apps
  are listed in `native_manifests()` in `server/src/packages.rs` (each app's `CONNECTOR_MANIFEST`: CRM, Helpdesk, HR,
  Projects, Assets, Commerce, Bookings, LMS, ITSM, Control, Objectives).
- **A suite** (`suite`) is a `meta` app with only the apps it requires: no packages, no interface, no
  contributions. Installing it installs them. The parser refuses a suite that is anything else, and an app in
  category `suite` that is not one (`a suite (category suite) is a meta app with only the apps it requires`). The
  platform's suites are in `server/apps/suites/*.yaml`.

### An extension build (a server of the app's own)

An app that the platform's repository may not name (a private one, or one whose code the platform's public CI
cannot fetch) builds its own server: a binary in its own repository that depends on `ferrum-server`, adds the app
with `ferrum_server::extensions::extend` and calls `ferrum_server::run` (`server/src/extensions.rs`;
`docs/plan/apps.md` §11). The `Extension` struct carries what the `apps!` list carries for a compiled-in app, plus
the app's `ferrum task` tasks and its `/metrics` lines.

Cloud is the example: `ferrum-app-cloud/server/src/main.rs` builds `ferrum-cloud`, the platform's server with Cloud
and Code inside, each registered as itself:

```rust
fn main() -> loco_rs::Result<()> {
    extensions::extend(apps()); // Cloud and Code, each an `Extension { id, packages, register, reports, maintenance, manifest, ui, tasks, metrics }`
    ferrum_server::run()
}
```

The platform holds itself to this with `server/tests/extensions_it.rs`. The app's own CI builds and tests that
server; the platform's CI never fetches it.

### A plugin (a `wasm` app)

A WebAssembly module plus `plugin.json`: hooks inside a document's transaction, HTTP routes (signed in or
public), WebSockets, event subscriptions, schedules, and HTTPS calls to declared hosts, all inside a sandbox with
only the permissions an administrator approved ([docs/plugins.md](../plugins.md)). It is for code that is not
reviewed with the platform: an integration, a gateway, a vertical's logic. It cannot open raw sockets, run SQL or
keep a table of its own; private apps' entities live in the generic custom-record store
([docs/plan/apps.md](../plan/apps.md) §5).

Examples: `plugins/examples/` in the platform (`after-commit`, `connector-demo`, `device-gateway`,
`discount-guard`), and the Commerce app's payment gateways (`gateways/` in its repository). The SDK is
`ferrum-plugin-sdk` in the `ferrum-sdk` repository.

### A service app

Code on its publisher's servers (`runtime: service`, a `service: {url, events}` section). Installing gives it an
account of its own whose role is exactly `access`, and an API key; it hears of records through signed webhooks,
and cannot veto a save ([docs/plugins.md](../plugins.md), *Services on their own servers*). No first-party app uses
this kind today.

## One repository per app

Every app lives in a repository of its own, `ferrum-app-<name>`, under a licence Ferrum may ship (the first row of
[docs/app-contract.md](../app-contract.md)). Never in the platform's `crates/`.

`scripts/check_apps.py` reads `cargo metadata` and checks every package whose source is
`git+https://github.com/CBFerrum/ferrum-app-…` and has an `app.yaml` beside its `Cargo.toml`. An app inside the
platform's own workspace is not one of those, so the check never sees it.

> **Two exceptions exist.** Operations (`crates/ops`, `id: ops`) and Studio (`crates/studio`,
> `id: studio`) are apps with an `app.yaml` inside the platform's `crates/`. Neither has a `CHANGELOG.md`,
> `LICENSE`, `homepage` or `support` of its own, and `scripts/check_apps.py` does not check them. A new app does not
> follow them.

### The layout

The layout of a native app, from GRC (`ferrum-app-grc`), the smallest first-party app with packages, an
interface, a scenario test and upkeep:

```
ferrum-app-grc/
  app.yaml                  the manifest (03-the-manifest.md)
  Cargo.toml                the crate, ferrum-app-grc; depends on ferrum-kernel, ferrum-meta, ferrum-domain by git
  build.rs                  embeds packages/ and ui/dist into the crate
  src/lib.rs                MANIFEST, packages(), register(), reports(), maintenance()
  src/risks.rs …            the hooks, one module per area
  packages/grc/package.yaml the package: modules, roles, workflows, seeds, personal data
  packages/grc/entities/    the entities (04-data-and-records.md)
  packages/grc/sql/         an app's own tables (GRC has none; Drive has one file)
  tests/grc_it.rs           the scenario test against a real PostgreSQL (15-testing-and-ci.md)
  ui/                       the interface's sources; ui/dist is built and committed (17-the-interface.md)
  rustfmt.toml              max_width 160 (02-your-first-app.md)
  README.md CHANGELOG.md LICENSE docs/   (22-readme-and-repository.md)
  .github/workflows/ci.yml  the workflow scripts/app_ci.sh replays on the developer's machine
  .gitignore                /target, .cargo/config.toml, Cargo.lock (a library crate), ui/node_modules
```

What a bundle (`.fapp`) may hold is fixed and anything else is refused, not ignored (`crates/apps/src/lib.rs`,
*Bundle format*; `kind()` in `crates/apps/src/bundle.rs`): `app.yaml`, `README.md`, `CHANGELOG.md`, `LICENSE`,
`plugin.json`, `plugin.wasm`, `packages/<name>/` (`package.yaml`, `README.md`, `entities/*.yaml`, `sql/*.sql`,
`print/*.html`, `print/*.layout.json`), images under `assets/`, screens under `ui/`, and an industrial solution's
`industrial/profiles/*.yaml` and `industrial/acceptance/*.yaml`. A native app is not shipped as a bundle to the
server (its code is compiled in), but the store takes its manifest and packages that way.

### Naming

| What | Rule | Where it is checked |
|---|---|---|
| Repository | `ferrum-app-<name>` | `scripts/check_apps.py` only looks at sources named so; `scripts/app_ci.sh` takes the app's name from it |
| App id | `[a-z][a-z0-9-]{1,39}`; not `ui` or `shell` | `is_app_id`, `validate_extensions` in `crates/apps/src/manifest.rs` |
| Crate | `ferrum-app-<id>` (Rust name `ferrum_app_<id>`) | reviewed, not checked |
| Package | `[a-z][a-z0-9_-]{0,62}` | `ferrum_meta::is_package_name` |
| Entity | PascalCase, `[A-Z][A-Za-z0-9]{0,62}`; a third-party app's starts with its `prefix`; `X…` is for custom and plugin entities | `ferrum_meta::is_entity_name`; the bundle checks the prefix |
| Hook name | `{app}.{what}` (`grc.policy`) | reviewed; a name another hook waits for must exist (`HookRegistry::settle`) |
| Route | `[a-z][a-z0-9-]{1,30}`, not one the core keeps | `RESERVED_ROUTES` in `crates/apps/src/shell.rs` |

### Licence

`check_apps.py` fails an app whose `license` does not start with one of `AGPL-3.0`, `GPL-3.0`, `LGPL-3.0`, `MIT`,
`Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `MPL-2.0` (`LICENSES` in `scripts/check_apps.py`), and one with no
`license` at all. Ferrum's own apps say `AGPL-3.0-only` and ship the text as `LICENSE`. What an app depends on is
held to the same list by `scripts/licenses.py` once the platform compiles it in: see
[13-security-rules.md](13-security-rules.md) and [16-releasing.md](16-releasing.md).

## What belongs in the platform, and what in an app

The platform holds what every app needs, and nothing that is one app's business:

| In the platform (`CBFerrum/Ferrum`) | In an app |
|---|---|
| The kernel: records, permissions, the document pipeline, the outbox, jobs, the vault, files, the audit trail (`crates/kernel`) | The records of one line of business, their hooks, their reports |
| The metadata format and its validator (`crates/meta`) | Packages written in that format |
| The records every app shares: companies, currencies, units (`packages/core`), people outside the workspace (`packages/contacts`), customers and suppliers (`packages/parties`), items and price lists (`packages/products`), projects (`packages/project_records`) | Fields an app adds to those, through overlays, with its own prefix where it is a third party |
| The shell, the records frame, the kit (Ferrum-Frontend) | Its own interface, built with the kit |
| General engines: approvals, workflows, numbering, messages, document templates, service targets, connections | The content those engines run: an approval policy, a message, a template, a country's tax table |

Depend on the least you need: `parties` for customers, `products` for items, ERP's packages only for ERP's own
records. Every package must work with only its dependencies installed: the catalog refuses one whose links,
overlays, workflows, seeds or personal-data rules reach further, and what reaches further belongs in a connector
(`Catalog` checks in `crates/meta/src/catalog.rs`; [docs/plugins.md](../plugins.md), *Records other apps share*).

When an app needs something the platform does not offer, the change goes to the platform first, in its own pull
request, with its tests; the app then raises its `platform` range to the release that has it. An app never patches
a platform crate in its own repository (the git-ignored `.cargo/config.toml` that `scripts/app_ci.sh` writes points
at a platform checkout for building, never at a modified copy kept with the app).

### Before building anything

Look for a mature open-source project to run, depend on or adapt before writing one, and take only what is
AGPL-compatible. GRC's risk model is adapted from CISO Assistant (AGPL-3.0) and says so in `docs/PROVENANCE.md`
and its crate's documentation; Cloud's forge is a real Forgejo, run beside the server, not a forge written
again ([docs/app-contract.md](../app-contract.md), *A companion's sign-in*). The platform keeps its readings in
`docs/references/`. Reviewed, not checked.

## Engines and content

An engine is general. What differs by country or sector is data the engine reads, or an add-on that brings it:

- **A country** is an add-on of ERP: `ferrum-app-erp/addons/india`, `addons/de`, `addons/countries` (Singapore,
  Malaysia, Japan, Mexico and the rest on ERP's VAT and GST engine). Each brings its package (tax templates,
  accounts, reports, fields with its own prefix) and its hooks. The server compiles them as features of their own
  (`india`, `de`, `sg`, … in `server/Cargo.toml`).
- **A sector** is an app of its own built on shared records (Healthcare, Education, Hospitality, Construction),
  or configuration in the builder ([docs/builder/README.md](../builder/README.md)).
- **Never a country in an engine's code.** No `if country == "IN"` in a general crate or a general app. Where a
  rule is a country's, it is a row, a template, a setting, or a hook in that country's add-on.
  [11-integrations.md](11-integrations.md) has how to split one. Reviewed, not checked.

## Checked by

- `crates/apps/src/manifest.rs` (`AppManifest::parse`, `validate`): runtimes, ids, the kind rules; its tests
  `plugin_rules`, `service_rules`, `dependency_and_package_rules`.
- `scripts/check_apps.py`: a repository of its own (only `ferrum-app-*` sources are checked), the manifest's
  fields, the licence, a description, a history, a scenario test, a built interface, a launcher icon.
- `server/src/packages.rs`, test `compiled_apps_fit_together`: every compiled app's manifest parses, contributions
  meet their points' schemas, every launcher icon exists, `requires` between compiled apps is met, no two apps share
  a report name or a route, and every package loads into one catalog.
- `server/tests/extensions_it.rs`: an extension build works.
- Reviewed, not checked: one repository per app for apps inside the platform's workspace, crate naming, engines kept
  general, looking for a mature project first.

## See also

- [02-your-first-app.md](02-your-first-app.md): the steps from an empty repository.
- [03-the-manifest.md](03-the-manifest.md): every field of `app.yaml`.
- [11-integrations.md](11-integrations.md): other apps' data, countries as add-ons.
- [22-readme-and-repository.md](22-readme-and-repository.md): what the repository holds besides code.
- [docs/app-contract.md](../app-contract.md), [docs/plan/apps.md](../plan/apps.md), [docs/plugins.md](../plugins.md),
  [docs/plan/apps-ui.md](../plan/apps-ui.md).
