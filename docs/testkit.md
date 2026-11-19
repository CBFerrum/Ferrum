# The test kit

An app's integration suite starts one way: from `ferrum-testkit` (`crates/testkit`), which boots a kernel as the server
boots one. Take it under `[dev-dependencies]` only: dev-dependencies are built for tests and never for a release, so
nothing of the kit, nor what it depends on, is in a server that ships.

What it does, as `server/src/app.rs` and `server/src/packages.rs` do:

- empties the suite's database, the vault's schema included, fifty tables to a statement;
- opens the vault before the kernel, with its credential slot made from the address's password (a cheap Argon2id);
- builds the catalog from the platform's packages (`ferrum_domain`) and each app's, leaving out a package that needs
  one the build lacks;
- registers the platform's hooks, then each app as itself (`HookRegistry::app`): ERP first and for every workspace, every
  other app only for the workspaces that installed one of its packages, each owning its packages and taking its own
  vault handle; then sets every app's reports at once, with who gives each;
- keeps mail and files in memory. It runs no app's upkeep: a suite calls `maintenance(k, tenant)` for the workspace it
  means, as the server's runner does (docs/app-contract.md, Upkeep).

It fails when the vault does not open.

## A suite on the kit

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
| `app!("id", crate)` | an app as the server lists it: its crate's `packages`, `register` and `reports` |
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
| `at_screen(&k, &ctx)` | that person at a screen in a request, as the server makes one for the browser's cookie: what a decision only a person makes asks for (BD-83), and every change to how a person signs in (BD-85); `ws.user` gives a context loaded outside a request, which is not |
| `ws.drain()` | dispatches events until none of this workspace's is left, waiting for those another scenario's dispatcher holds; fails after two minutes naming each event left and its last error |
| `obj(json)`, `uid(&doc)` or `uid(&doc["customer"])`, `dec(&value)` | a record from JSON, a record's id, a decimal (zero for null) |
| `refused(result, kind, says)` | refused as `invalid` (a validation error too) or as the error's code with spaces (`forbidden`, `not found`, `conflict`, …), its text saying `says` |

## What it does not do

- **Record wrappers.** `insert`, `get`, `list` with a suite's panics differ by whose context each acts in: keep them in
  the suite, over `kit.k`.
- **Jobs.** `ws.drain()` delivers events. A suite that needs jobs run calls `kit.k.run_jobs_once(n)`.
- **Two app sets in one test binary.** Reports are the process's: every boot in a binary gives the same apps.
- **Two databases at once.** Each boot empties its database. Scenarios that run side by side share one `Kit` (a
  `tokio::sync::OnceCell<Kit>` in the suite) and make a workspace each.

## Using it in an app's repository

1. `Cargo.toml`, `[dev-dependencies]`: `ferrum-testkit = { git = "https://github.com/CBFerrum/Ferrum", branch = "main" }`.
   For local development, `.cargo/config.toml`'s patch gets `ferrum-testkit = { path = "<platform>/crates/testkit" }`
   (`scripts/app_ci.sh` writes one line per platform crate, this one included).
2. Every app the suite registers is an `app!` entry, ERP and the siblings included: `app!("x", ferrum_app_x)` scopes its
   hooks to its packages as the server does. A suite that needs the app's vault handle from its registration takes
   `kit.vault("x")`.
3. The kit runs no app's upkeep. A scenario calls `ferrum_app_x::maintenance(&kit.k, ws.tenant)` where it needs one.
4. Events are delivered with `ws.drain().await`.
5. The variable a suite gives `database("FERRUM_X_TEST_DATABASE_URL")` is the one `scripts/app_ci.sh` finds: it gives it
   a database of its own (`ferrum_x_test_localci`).

Suites in this repository that start from the kit: `crates/kernel/tests/consent_it.rs` (a package of its own, the vault, mail),
`crates/kernel/tests/app_uses_it.rs` (one app using another's operation), and `crates/studio/tests/government_pack_it.rs`
(ERP and Studio, scheduled work, events drained).
