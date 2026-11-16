# The test kit

An app's integration suite starts one way: from `ferrum-testkit` (`crates/testkit`), which boots a kernel as the server
boots one. Take it under `[dev-dependencies]` only: dev-dependencies are built for tests and never for a release, so
nothing of the kit, nor what it depends on, is in a server that ships.

What it does, as `server/src/app.rs` and `server/src/packages.rs` do:

- empties the suite's database, the vault's schema included, a few dozen tables to a statement;
- opens the vault before the kernel, with its credential slot made from the address's password (a cheap Argon2id);
- builds the catalog from the platform's packages (`ferrum_domain`) and each app's, leaving out a package that needs
  one the build lacks;
- registers the platform's hooks, then each app as itself (`HookRegistry::app`): ERP first and for every workspace, every
  other app only for the workspaces that installed one of its packages, each owning its packages and taking its own
  vault handle; then sets every app's reports at once, with who gives each;
- keeps mail and files in memory, and runs each app's upkeep once.

It fails when the vault does not open.

## A suite before and after

Before (Construction's `tests/construction_it.rs`, about a hundred lines with its helpers):

```rust
let url = std::env::var("FERRUM_CONSTRUCTION_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_construction_test".into());
let db = Db::connect(DbConfig { url, max_connections: 16, ..Default::default() }).await.expect("connect");
sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset"); // the vault's schema stays
let mut packages = ferrum_domain::packages().expect("platform packages");
packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
packages.extend(ferrum_app_construction::packages().expect("app packages"));
let mut hooks = HookRegistry::default();
ferrum_domain::register(&mut hooks);
hooks.app("erp", &[], ferrum_app_erp::register);
ferrum_app_construction::register(&mut hooks);              // runs in every workspace, has no vault handle
let k = Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), None).await.expect("boot");
ferrum_app_erp::maintenance(&k).await.expect("partitions");
let tenant = k.create_tenant(NewTenant { slug: "brick".into(), /* six more fields */ }).await.expect("tenant");
let session = k.login("brick", "admin@brick.test", "Correct-Horse-9").await.expect("login");
let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
```

After:

```rust
use ferrum_testkit::{Kit, app, database, obj, refused, run, uid};

#[test]
fn a_building_job() {
    run(scenario());
}

async fn scenario() {
    let apps = [app!("erp", ferrum_app_erp), app!("construction", ferrum_app_construction)];
    let kit = Kit::boot(&database("FERRUM_CONSTRUCTION_TEST_DATABASE_URL"), &apps).await;
    let ws = kit.workspace("brick", &["construction"], json!({"fiscal_year_start_month": 4, "timezone": "Asia/Kolkata"})).await;
    let cm = ws.user("cm@brick.test", &["Contracts Manager"]).await;
    refused(kit.k.insert(&cm, "Estimate", obj(estimate("Nothing", json!([]), "10"))).await, "invalid", "lines");
    let est = kit.k.insert(&cm, "Estimate", obj(estimate("Kitchen", lines, "10"))).await.unwrap();
    refused(kit.k.run_action(&cm, "Estimate", uid(&est), "accept", Value::Null).await, "invalid", "not available");
    ws.drain().await;                                          // this workspace's events delivered, or the test fails naming them
}
```

## What it gives

| | |
|---|---|
| `run(scenario)` | the scenario as a task of one runtime per test binary, with the server's 16 MiB thread stacks; a kernel booted under one test serves the next |
| `database("FERRUM_X_TEST_DATABASE_URL")` | the variable's address, else `ferrum_x_test` on the local server as `ferrum` (the kit empties it) |
| `app!("id", crate)` | an app as the server lists it: its crate's `packages`, `register`, `reports` and `maintenance` |
| `Kit::boot(url, &apps)` | the database emptied and a kernel booted on it, as above |
| `Kit::boot_with(url, &apps, packages, settings)` | with packages no app brings (a stand-in for another app's package, a package as it was) and the kernel's settings (`public_url`, `webhooks_allow_private`, `pdf_url`) |
| `kit.k` | the kernel |
| `kit.workspace(slug, &packages, settings)` | a workspace with those packages (and what they depend on) and settings (`Value::Null` for none); its administrator `admin@{slug}.test` signed in |
| `kit.replica()` | another server on the same database, started the same way and nothing emptied: whichever server is asked, a restart |
| `kit.vault("id")` | the app's own vault handle, as its registration took it |
| `kit.mail()` | the mail sent so far: to, subject, html, files attached |
| `kit.files` | the files stored (`FileStore::get` by key) |
| `ws.admin`, `ws.tenant`, `ws.slug`, `ws.k` | the workspace |
| `ws.user(email, &roles)` | a person holding those roles, signed in with `PASSWORD` |
| `ws.drain()` | dispatches events until none of this workspace's is left, waiting for those another scenario's dispatcher holds; fails after two minutes naming each event left and its last error |
| `obj(json)`, `uid(&doc)` or `uid(&doc["customer"])`, `dec(&value)` | a record from JSON, a record's id, a decimal (none for null) |
| `refused(result, kind, says)` | refused as `invalid` (a validation error too) or as the error's code with spaces (`forbidden`, `not found`, `conflict`, …), its text saying `says` |

## What it does not do

- **Record wrappers.** `insert`, `get`, `list` with a suite's panics differ by whose context each acts in: keep them in
  the suite, over `kit.k`.
- **Jobs.** `ws.drain()` delivers events. A suite that needs jobs run calls `kit.k.run_jobs_once(n)`.
- **Two app sets in one test binary.** Reports are the process's: every boot in a binary gives the same apps.
- **Two databases at once.** Each boot empties its database. Scenarios that run side by side share one `Kit` (a
  `tokio::sync::OnceCell<Kit>` in the suite) and make a workspace each.

## Adopting it in an app's repository

1. `Cargo.toml`, `[dev-dependencies]`: `ferrum-testkit = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }`.
   For local development, `.cargo/config.toml`'s patch gets `ferrum-testkit = { path = "<platform>/crates/testkit" }`
   (`scripts/app_ci.sh` writes one line per platform crate, this one included).
2. In each suite, the block from `Db::connect` to `load_ctx` becomes `Kit::boot` and `kit.workspace`. Every app the suite
   registered is an `app!` entry, ERP and the siblings included; a bare `ferrum_app_x::register(&mut hooks)` becomes
   `app!("x", ferrum_app_x)`, which scopes its hooks to its packages as the server does. A suite that kept the app's
   vault handle from inside its registration takes `kit.vault("x")`.
3. The suite's own `obj`, `uid`, `dec`, `refused`, `user`, `drain`, in-memory file store, mail capture and runtime builder
   go; it imports the kit's. A helper of the same name that means something else stays (a `dec` giving `f64`).
4. `KernelSettings` go to `Kit::boot_with`. `secret_key` goes: the kit refuses it.
5. `ferrum_domain::maintenance(&k)` and `ferrum_app_erp::maintenance(&k)` after boot go: the kit runs every app's upkeep
   once. Calls later in a scenario stay.
6. `while k.dispatch_outbox_once().await? > 0 {}` becomes `ws.drain().await`.
7. The variable names stay. `scripts/app_ci.sh` finds each one a suite gives `database("…")`, as it found the suites'
   own defaults, and gives it a database of its own (`ferrum_x_test_localci`). Run the suite.

Converted so far: `crates/kernel/tests/consent_it.rs` (a package of its own, the vault, mail) and
`crates/studio/tests/government_pack_it.rs` (ERP and Studio, scheduled work, events drained).
