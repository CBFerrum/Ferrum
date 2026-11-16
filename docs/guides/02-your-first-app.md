# Your first app

From an empty repository to a native app the platform builds, with its scenario test, its interface and its
checks. The example is a small app, **Site Visits** (`visits`): a visit to a customer's site, planned, then done
with its findings. Every file below is modelled on Governance and Risk (`ferrum-app-grc`), the smallest
first-party app that has packages, hooks, upkeep, an interface and a scenario test; each step names the GRC file it
follows, so when this guide and GRC differ, read GRC.

Nothing in this guide was run while it was written. The commands are the ones the scripts and workflows in the
platform and in GRC use; the output shown is what those scripts print, read from their code.

## What you need

| Tool | Version | Why |
|---|---|---|
| Rust | 1.95 or later (`README.md` of the platform) | the crate |
| PostgreSQL | 17 or later | the scenario test and a development server |
| Node | 22 or later | the interface |
| A platform checkout | `CBFerrum/Ferrum` | the crates the app builds against, and `scripts/app_ci.sh` |
| A Frontend checkout | `CBFerrum/Ferrum-Frontend`, with `npm install` done | the kit and the build tool the interface uses |

The interface builds from the Frontend at `../../ferrum/web` relative to the app's `ui/` (the packages are not on
npm yet: [docs/plugins.md](../plugins.md), *Building one*). Lay the checkouts out so that path exists:

```
work/
  ferrum/           the platform (CBFerrum/Ferrum)
  ferrum/web/       the Frontend (CBFerrum/Ferrum-Frontend), npm install done
  ferrum-app-visits/
```

`scripts/app_ci.sh` builds its own copy of this layout in a temporary folder, so the checks pass wherever your
checkouts are; the layout matters for running `npm` in `ui/` by hand.

A database role that is **not a superuser**: superusers bypass row-level security, so a test run as one proves
nothing about isolation (`scripts/app_ci.sh`, header; GRC's `.github/workflows/ci.yml`).

```bash
createuser ferrum --pwprompt                 # password: ferrum
createdb -O ferrum ferrum_visits_test
```

## 1. The repository

```bash
mkdir ferrum-app-visits && cd ferrum-app-visits && git init
```

The name is `ferrum-app-<id>`: `scripts/check_apps.py` only checks sources named so, and `scripts/app_ci.sh`
takes the app's id from the folder's name ([01-what-an-app-is.md](01-what-an-app-is.md)).

`ferrum-app new visits --runtime native --ui` (the `ferrum-app` command of the `ferrum-sdk` repository,
`cli/src/scaffold.rs`) writes a manifest, README, CHANGELOG, LICENSE, `.gitignore`, a package with one entity
and the interface's sources. It does not write the Rust crate (`Cargo.toml`, `build.rs`, `src/lib.rs`) nor a
scenario test, and its licence is MIT. The steps below write every file by hand, so that nothing is left out.

## 2. The manifest: `app.yaml`

After GRC's `app.yaml`:

```yaml
id: visits
name: Site Visits
version: 0.1.0
publisher: cbferrum
summary: "Visits to customers' sites, planned and done, with what was found"
description: README.md
categories: [projects]
license: AGPL-3.0-only
homepage: https://github.com/CBFerrum/ferrum-app-visits
support: https://github.com/CBFerrum/ferrum-app-visits/issues
runtime: native
platform: ">=0.3.2, <0.4"
packages: [visits]
uninstall: ask
# Its own interface at /visits/.
ui:
  dist: ui/dist
contributes:
  shell.app:
    - id: visits
      route: visits
      title: Site Visits
      description: Visits planned and done
      icon: calendar-check
      color: "#0f766e"
      roles: [Visit Planner]
  shell.open:
    - {id: visits, package: visits, path: "visits/e/{entity}/{id}", list: "visits/e/{entity}"}
  shell.create:
    - {id: visit, label: Visit, path: visits/e/SiteVisit/new, entity: SiteVisit}
```

Field by field, with what each check refuses, is [03-the-manifest.md](03-the-manifest.md). Three things to
get right now:

- `platform: ">=0.3.2, <0.4"`: an interface of its own needs 0.3.2 ([docs/plugins.md](../plugins.md), *Interfaces
  of their own*). If the app will use its vault handle or `operator_settings`, it is `">=0.3.9, <0.4"`
  (`PLATFORM_VERSION` in `crates/apps/src/lib.rs`).
- `icon` must be one the shell draws: `scripts/check_apps.py` reads the names from `const ICONS` in
  `server/shell/runtime.js` and fails `the shell draws no <name> icon`.
- `version` must appear in `CHANGELOG.md` (step 7).

## 3. The package: `packages/visits/`

After GRC's `packages/grc/package.yaml`:

```yaml
# packages/visits/package.yaml
package: visits
version: 0.1.0
label: Site Visits
description: "Visits to customers' sites, planned and done, with what was found"
depends: [core, parties]

modules:
  - name: visits
    label: Site Visits
    icon: calendar-check
    order: 60
    items: [SiteVisit]

roles:
  - name: Visit Planner
    description: Plans visits, records them done, and reads every visit.
    permissions:
      - {entity: SiteVisit, read: true, write: true, create: true, export: true}
  - name: Visitor
    description: Reads and records the visits they are named on.
    permissions:
      - {entity: SiteVisit, read: true, write: true, if_owner: true}
```

`depends` names the packages whose entities this one links to: `parties` for `Customer`. A link to an entity of a
package not in `depends` is refused when the catalog loads ([docs/plugins.md](../plugins.md), *Records other apps
share*).

After GRC's `packages/grc/entities/grc.yaml` (the `Finding` and `Policy` entities):

```yaml
# packages/visits/entities/site_visit.yaml
- name: SiteVisit
  label: Site Visit
  label_plural: Site Visits
  kind: master
  module: visits
  title_field: subject
  description: A visit to a customer's site, planned for a day, then done with what was found.
  owner_fields: [visitor]
  fields:
    - {name: subject, type: data, required: true, in_list: true}
    - {name: customer, type: link, to: Customer, required: true, in_list: true, in_filter: true}
    - {name: visitor, type: email, required: true, in_list: true, in_filter: true}
    - {name: planned_on, type: date, required: true, in_list: true}
    - {name: status, type: select, options: [Planned, Done, Cancelled], default: Planned, read_only: true, no_copy: true, in_list: true, in_filter: true}
    - {name: findings, type: text}
    - {name: done_on, type: date, read_only: true, no_copy: true, in_list: true}
  actions:
    - {name: mark_done, label: Mark It Done, permission: write, condition: "doc.status == 'Planned'"}
  numbering: {template: "VIS-{YYYY}-{seq:4}"}
  list: {columns: [number, subject, customer, visitor, planned_on, status], sort: planned_on desc}
  search_fields: [subject]
```

What each line means is [04-data-and-records.md](04-data-and-records.md). `owner_fields: [visitor]` lets the
`Visitor` role's `if_owner` rule reach the visits that name the person, not only those they created
([05-permissions-and-people.md](05-permissions-and-people.md)).

## 4. The crate

`Cargo.toml`, after GRC's (the platform crates by git, from `main`; the platform's own build unifies them with
its workspace through `[patch]`, and the developer's machine through a git-ignored `.cargo/config.toml`, step 9):

```toml
[package]
name = "ferrum-app-visits"
version = "0.1.0"
edition = "2024"
license = "AGPL-3.0-only"
description = "Visits to customers' sites, planned and done, with what was found"
repository = "https://github.com/CBFerrum/ferrum-app-visits"
build = "build.rs"

[dependencies]
ferrum-kernel = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }
ferrum-meta = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }
ferrum-domain = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }
async-trait = "0.1"
chrono = { version = "0.4", features = ["serde"] }
serde_json = "1"
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "tls-rustls-ring-webpki", "postgres", "uuid", "chrono", "rust_decimal", "json", "migrate", "macros"] }
uuid = { version = "1.10", features = ["v4", "v7", "serde"] }

[dev-dependencies]
ferrum-apps = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }
tokio = { version = "1.45", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }

[profile.dev]
debug = "line-tables-only"
```

`build.rs`: copy GRC's `build.rs` unchanged. It writes `packages.rs` (every file under `packages/<name>/` as
`EMBEDDED: &[(package, path, content)]`) and `ui.rs` (every file under `ui/dist`, dotfiles left out, as
`UI: &[(path, bytes)]`) into `OUT_DIR`, and reruns when either folder changes.

`src/lib.rs`, after GRC's:

```rust
//! Site visits: planned for a day, then done with what was found.

mod visits;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));
include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks.
pub fn register(hooks: &mut HookRegistry) {
    hooks.register("SiteVisit", Arc::new(visits::VisitHook));
}

/// This app's reports: none yet.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

/// Upkeep: none yet.
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}
```

`src/visits.rs`, after `src/policies.rs` and `src/risks.rs` of GRC: a check on save and an action.

```rust
use async_trait::async_trait;
use chrono::Utc;
use ferrum_domain::util::{elevated, id, s};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use serde_json::{Value, json};

pub struct VisitHook;

#[async_trait]
impl DocHook for VisitHook {
    fn name(&self) -> &'static str {
        "visits.visit"
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "status") == Some("Done") && s(doc, "findings").is_none_or(|f| f.trim().is_empty()) {
            return Err(Error::field("findings", "say what was found before the visit is done"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "mark_done" {
            return Ok(None);
        }
        let visit = id(doc, "id").ok_or_else(|| Error::internal("a visit without an id"))?;
        if s(doc, "findings").is_none_or(|f| f.trim().is_empty()) {
            return Err(Error::invalid("say what was found before the visit is done"));
        }
        let today = Utc::now().date_naive().to_string();
        // `status` and `done_on` are read-only to people; the engine sets them, in the action's transaction.
        let values = json!({"status": "Done", "done_on": today});
        hc.kernel.set_values_tx(&elevated(hc.ctx), hc.tx, "SiteVisit", visit, values.as_object().cloned().unwrap_or_default()).await?;
        Ok(Some(json!({"status": "Done"})))
    }
}
```

What the hooks may do, and the transaction they run in, is [04-data-and-records.md](04-data-and-records.md) and
[06-events-jobs-and-logic.md](06-events-jobs-and-logic.md). The kernel records `action:mark_done` on the audit
trail before the handler runs; `set_values_tx` writes derived values with no hooks or audit of its own
(`crates/kernel/src/doc.rs`).

`rustfmt.toml`, as GRC's and the platform's: `max_width = 160`, `use_small_heuristics = "Max"`.

`.gitignore`, as GRC's:

```
/target
.cargo/config.toml
.DS_Store
*.log
Cargo.lock
ui/node_modules
```

A library crate does not commit `Cargo.lock`; an app that ships a binary (Cloud) does, and `scripts/app_ci.sh` puts
a committed lock back as it was after a run.

## 5. The scenario test: `tests/visits_it.rs`

`scripts/check_apps.py` fails an app with no `tests/*_it.rs` ("no scenario test"). The test runs the app's flows
against a real PostgreSQL, as GRC's `tests/grc_it.rs` does: it resets the database, loads the platform's packages
and the app's into one catalog, registers the hooks as the server does, boots a kernel, makes a workspace with the
app's package, and works as people with the app's roles.

```rust
//! Site visits against a real PostgreSQL (`FERRUM_VISITS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_visits_test`; reset first).

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[test]
fn a_visit_is_planned_then_done() {
    // A thread stack as GRC gives its scenario: the kernel's futures are deep.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_VISITS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_visits_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 16, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");

    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_app_visits::packages().expect("app packages"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    // As the server registers it (server/src/packages.rs): as itself, owning its packages.
    let own: Vec<String> = ferrum_app_visits::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("visits", &own, ferrum_app_visits::register);
    hooks.own("visits", &own);
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");

    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme Ltd".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "parties".into(), "visits".into()],
            settings: Some(json!({"timezone": "UTC"})),
        })
        .await
        .expect("tenant");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let id = k.create_user(&admin, "pat@acme.test", "pat@acme.test", Some("Correct-Horse-9"), &["Visitor".to_string()], false).await.expect("user");
    let s = k.login("acme", "pat@acme.test", "Correct-Horse-9").await.expect("login");
    let pat = k.load_ctx(tenant, id, Some(s.token_version), "it".into()).await.expect("ctx");

    let customer = k.insert(&admin, "Customer", obj(json!({"customer_name": "Northwind"}))).await.expect("customer");
    let visit = k
        .insert(&admin, "SiteVisit", obj(json!({"subject": "Boiler check", "customer": customer["id"], "visitor": "pat@acme.test", "planned_on": "2026-11-20"})))
        .await
        .expect("visit");
    let vid = visit["id"].as_str().unwrap().parse().unwrap();

    // Done needs its findings: refused with the words a person reads.
    let r = k.run_action(&pat, "SiteVisit", vid, "mark_done", Value::Null).await;
    assert!(matches!(&r, Err(Error::Invalid(m)) if m.contains("say what was found")), "{r:?}");

    // The visitor writes the findings on their own visit, and marks it done.
    k.update(&pat, "SiteVisit", vid, obj(json!({"findings": "Pressure low; valve replaced"})), None).await.expect("findings");
    k.run_action(&pat, "SiteVisit", vid, "mark_done", Value::Null).await.expect("done");
    assert_eq!(k.get(&admin, "SiteVisit", vid, false).await.unwrap()["status"], "Done");

    // Another visitor's visit is not theirs: not found, not "forbidden" (05-permissions-and-people.md).
    let other = k
        .insert(&admin, "SiteVisit", obj(json!({"subject": "Roof", "customer": customer["id"], "visitor": "sam@acme.test", "planned_on": "2026-11-21"})))
        .await
        .expect("visit");
    let r = k.get(&pat, "SiteVisit", other["id"].as_str().unwrap().parse().unwrap(), false).await;
    assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
}

#[test]
fn the_interface_is_a_release_the_platform_takes() {
    let m = ferrum_apps::AppManifest::parse(ferrum_app_visits::MANIFEST).expect("app.yaml");
    assert_eq!(m.routes(), vec![("visits".to_string(), vec![])]);
    let files = ferrum_app_visits::UI.iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect();
    ferrum_apps::ui::prepare("visits", files).expect("ui/dist is an interface release");
}
```

The second test is GRC's `the_interface_is_a_release_the_platform_takes` (in GRC it sits in `src/lib.rs`): it
parses the manifest with the very parser the server uses, and prepares `ui/dist` as the server will at start.

The names (`customer_name` on `Customer`, the `Correct-Horse-9` password GRC's tests use) come from the
platform's `packages/parties` and from GRC's test. What a good scenario covers (each role, every refusal with
the words a person reads, upkeep run at a chosen day as GRC's `maintenance_on` is, the vault opened when the app
uses it) is [15-testing-and-ci.md](15-testing-and-ci.md).

## 6. The interface: `ui/`

Copy GRC's `ui/` files and change the app's names in them:

| File | What it holds (GRC's) |
|---|---|
| `ui/package.json` | `build`, `check`, `typecheck`, `test`, each `node ../../ferrum/web/packages/build/cli.mjs <command>` |
| `ui/vite.config.mjs` | `export default ferrumApp(import.meta.url);` from `../../ferrum/web/packages/build/vite.mjs` |
| `ui/tsconfig.json` | extends `../../ferrum/web/packages/build/tsconfig.app.json` |
| `ui/index.html` | `<html lang="en">`, a title, the viewport, one module script `./src/main.tsx` |
| `ui/src/main.tsx` | the router: `/` the app's home, `e/:entity`, `e/:entity/:id`, `r/:name` from the records frame, `basename: routeBase("visits")`, `shell().ready()` once drawn, `setFrameMode("app", ["visits"])` |
| `ui/src/records.ts` | re-exports `FormView`, `ListView`, `ReportView` from `@ferrum/records` and `AppSettingsPage`, `ScreenPage` from `@ferrum/records/frame` |
| `ui/src/Home.tsx` | the home: what needs attention, and the next step when there is nothing yet |
| `ui/src/*.test.ts` | the interface's tests (vitest) |
| `ui/ui-lint-baseline.json` | `{}`: the lint's ratchet, empty for a new app |

Build it, and commit `ui/dist` (the crate embeds it at compile time):

```bash
cd ui
npm run build        # vite build into dist/, then dist/.source, dist/words.json and dist/names/
npm run check        # fails unless dist/ is built from these sources, keeps the size budgets and passes the lint
npm run typecheck
npm test
```

`check` fails when `dist/` was not built from the sources as they are now, when the built CSS lacks the kit's base,
when a catalog holds words the sources do not ask for, when the first script is over 200 KB compressed or the CSS
over 50 KB, and when the UI lint finds more of a rule than `ui-lint-baseline.json` allows
(`packages/build/cli.mjs` of the Frontend, header). `check_apps.py` fails an app whose `ui.dist` has no
`index.html` or no `.source` (`ui/dist is not built (index.html and .source)`).

How to write the pages themselves is [17-the-interface.md](17-the-interface.md) and
[18-accessibility.md](18-accessibility.md).

## 7. The documents

| File | What it must hold | Checked by |
|---|---|---|
| `README.md` | what the app does, in the toolkit's form; it points at `CHANGELOG.md` | `check_apps.py`: the manifest names it; it points at `(CHANGELOG.md)` or lists the version in a changelog table |
| `CHANGELOG.md` | a `## 0.1.0` heading, then what that version did | `check_apps.py`: `CHANGELOG.md says nothing of 0.1.0` |
| `LICENSE` | the AGPL-3.0 text | the licence's name is checked in the manifest; the file is reviewed |
| `SECURITY.md`, `docs/` | how to report a vulnerability; the app's own documentation | reviewed |

The README's sections, banner and comparison table are [22-readme-and-repository.md](22-readme-and-repository.md);
what a README may never claim is there and in [20-writing-for-people.md](20-writing-for-people.md).

```markdown
# Changelog

## 0.1.0

First release: visits planned for a day and done with their findings; planners see every visit, visitors their own.
```

## 8. The workflow: `.github/workflows/ci.yml`

Copy GRC's. `scripts/app_ci.sh` reads it to replay the same steps on your machine: it takes the test database's
name from the line `FERRUM_<APP>_TEST_DATABASE_URL: postgres://…/<db>` and the thread stack from `RUST_MIN_STACK`.
For Visits the line is:

```yaml
    env:
      CARGO_TERM_COLOR: always
      FERRUM_VISITS_TEST_DATABASE_URL: postgres://ferrum:ferrum@localhost:5432/ferrum_visits_test
```

GitHub's workflows are switched off on Ferrum's repositories; checks run on the developer's machine. The file stays
because the script reads it. See [15-testing-and-ci.md](15-testing-and-ci.md).

## 9. Run the checks

From the platform checkout:

```bash
scripts/app_ci.sh ../ferrum-app-visits
```

What it does (`scripts/app_ci.sh`), in order, and what it prints:

1. Writes `.cargo/config.toml` in the app (only when there is none), patching every platform crate to this checkout:
   `[patch."https://github.com/CBFerrum/Ferrum"]` with one `ferrum-<crate> = { path = … }` line per crate. It refuses
   to go on if that file is not git-ignored (`.cargo/config.toml is not ignored here: refusing to leave it`).
2. `== fmt`: `cargo fmt --all -- --check`.
3. `== clippy`: `cargo clippy --all-targets -- -D warnings`.
4. `== test (ferrum_visits_test_localci)`: drops and makes the database `<name>_localci` owned by `ferrum` (never
   one named like `ferrum_dev`, a walk's or `ferrum_test`: it exits 2 rather than reset it), runs `cargo test
   --no-fail-fast` with `FERRUM_VISITS_TEST_DATABASE_URL` pointing there, prints the `test result:` lines (or the
   last 60 lines of a failure), deletes the test binaries and drops the database.
5. `== ui`: copies the checkout beside a link to the Frontend and runs `cli.mjs check`, `typecheck` and `test`.
6. `== PASS: visits` and exit 0, or `== FAIL: visits` and exit 1.

`NO_DEBUG=1` builds the tests without debug information in a target of their own, deleted afterwards, for a
machine short of disk. `PG`, `PLATFORM` and `FRONTEND` move the database, the platform and the Frontend.

`scripts/check_apps.py` runs over the apps the platform's lock compiles in, so it sees Visits only once the
platform names it (below). Until then, read its list against your repository by hand: `app.yaml` has `id`,
`name`, `version`, `publisher`, `summary`, `categories`, `license`, `homepage` and `support`; the description file
exists; `CHANGELOG.md` has the version; `tests/*_it.rs` exists; `ui/dist/index.html` and `ui/dist/.source` exist;
the launcher's icon is the shell's. Once compiled in, it prints one line per app (`ferrum-app-visits: ok`, or what
it lacks joined by `; `) and `N apps, M short of the contract; K manifests whose operator settings are not sound`.

## 10. Run it on a development server

The platform's quick start (`README.md`, *Quick start (development)*), with the app compiled in:

```bash
createdb -O ferrum ferrum_dev
cd server && cargo run -- start         # http://127.0.0.1:5150: migrates and syncs the schema at boot
cd server && cargo run -- task create_tenant slug:demo name:"Demo Ltd" email:admin@demo.test password:'Correct-Horse-9' packages:core,parties,visits
```

Sign in at `http://127.0.0.1:5150/` with tenant `demo`, `admin@demo.test`; the launcher shows Site Visits at
`/visits/`. To work on the interface with hot reload, start the server with `FERRUM_DEV_UI=true` (development only;
the server warns at start), run `npm run dev` in `ui/` (Vite on port 5173), and `ferrum-app dev --ui
http://localhost:5173 --no-upload`: the app's pages then come from Vite behind the real session, bar and bootstrap,
until `--ui off` or a restart ([docs/operations.md](../operations.md), *Apps with interfaces of their own*).

A server for walking flows in a browser keeps its own database and port, never a shared one.

## Putting it in the platform

The platform compiles an official native app in as a cargo feature pointing at its repository. Four places in the
platform's repository, each as GRC is there:

| File | What to add | GRC's line |
|---|---|---|
| `server/Cargo.toml`, `[dependencies]` | the crate by git, optional | `ferrum-app-grc = { git = "https://github.com/CBFerrum/ferrum-app-grc", branch = "main", optional = true }` |
| `server/Cargo.toml`, `[features]` | the feature, and its name in `default` | `grc = ["dep:ferrum-app-grc"]` |
| `server/src/packages.rs`, `apps!` `native { … }` | the feature and the crate | `"grc" => ferrum_app_grc,` |
| `server/src/packages.rs`, `native_uis()` | its interface, when it has one | `#[cfg(feature = "grc")] out.push(("grc", ferrum_app_grc::UI));` |

`Cargo.lock` then pins the commit of `main` the platform builds; moving it forward is pinning a new release
([16-releasing.md](16-releasing.md)). The platform's test `compiled_apps_fit_together` (`server/src/packages.rs`)
then holds the app to the others: its manifest parses, its icon exists, what it requires of another compiled app
is met by the release compiled with it, no report name or route is shared, and its packages load into one catalog
with everyone's. `every_report_is_gated_on_its_own_apps_entity` in the same file holds its reports to its own
entities. `scripts/check_apps.py`, `scripts/mutation_paths.py` and `scripts/secret_paths.py` start reading its
sources.

An app the platform's repository may not name builds its own server instead
([01-what-an-app-is.md](01-what-an-app-is.md), *An extension build*).

## What goes wrong first

| What you see | Why | What to do |
|---|---|---|
| `app.yaml: unknown field …. This server does not know that field, and will not ignore what may be a requirement: this app needs a newer Ferrum` | a field misspelt, or one a newer platform has | spell it as [03-the-manifest.md](03-the-manifest.md) does, or raise `platform` |
| `contributes.shell.app.visits: a route needs the app's own interface (a ui section)` | `route` without `ui` | add `ui: {dist: ui/dist}`, or use `entity` for a records-only entry |
| `the shell draws no … icon` | an icon `server/shell/runtime.js` does not have | pick one from its `ICONS` |
| `ui/dist is not built (index.html and .source)` | `npm run build` not run, or `dist/` not committed | build and commit `ui/dist` |
| `CHANGELOG.md says nothing of 0.1.1` | the version moved without a heading | add `## 0.1.1` |
| the test passes against a superuser and fails in `app_ci.sh` | row-level security was bypassed | test as the `ferrum` role |
| `refusing to reset ferrum_dev_localci` | the workflow names a database like a development one | name the test database `ferrum_<app>_test` |
| `package SQL visits/001_x.sql changed after it was applied; add a new file instead of editing it` | an applied SQL file edited | put the change in a new file ([04-data-and-records.md](04-data-and-records.md)) |

## Checked by

- `scripts/app_ci.sh`: formatting, clippy with warnings as errors, the tests against a non-superuser database, the
  interface's `check`, `typecheck` and `test`.
- `scripts/check_apps.py`: the manifest's fields, the description, the history, the scenario test, the built
  interface, the launcher icon (once the platform compiles the app in).
- `server/src/packages.rs` tests `compiled_apps_fit_together` and `every_report_is_gated_on_its_own_apps_entity`.
- The app's own test that parses `MANIFEST` and prepares `UI` (`ferrum_apps::AppManifest::parse`,
  `ferrum_apps::ui::prepare`).

## See also

- [03-the-manifest.md](03-the-manifest.md), [04-data-and-records.md](04-data-and-records.md),
  [05-permissions-and-people.md](05-permissions-and-people.md).
- [15-testing-and-ci.md](15-testing-and-ci.md), [16-releasing.md](16-releasing.md),
  [17-the-interface.md](17-the-interface.md), [22-readme-and-repository.md](22-readme-and-repository.md).
- GRC's repository: `ferrum-app-grc` (`app.yaml`, `build.rs`, `src/lib.rs`, `tests/grc_it.rs`, `ui/`).
