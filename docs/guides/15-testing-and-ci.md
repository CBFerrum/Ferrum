# Testing and CI

An app is tested against a real PostgreSQL, booted the way the server boots it, with the vault open, and its checks run
on the developer's machine before anything is pushed. This guide says how to write the suite, how to know a test proves
something, how to test against the real programs an app talks to, and what each check script checks.

## The suite

The contract requires at least one scenario test under `tests/` that runs the app's flows against a real PostgreSQL
([docs/app-contract.md](../app-contract.md)); `scripts/check_apps.py` fails an app with no `tests/*_it.rs` (an add-on's
may be its parent's). In practice each area of an app has one: Comms has `comms_it.rs`, `arrivals_it.rs`,
`platform_it.rs` and `services_it.rs`; Metrics seven.

### A real PostgreSQL, a throwaway database

- **The role.** The tests connect as an ordinary role, `ferrum`, that owns its test database and is no superuser:
  superusers bypass row-level security, so a suite run as one proves nothing about tenant isolation. The app's workflow
  makes it so (`create role ferrum login password 'ferrum'; create database ferrum_comms_test owner ferrum;`).
- **The address.** Each suite reads its database from a variable of its own, with a default:
  `std::env::var("FERRUM_METRICS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_metrics_test".into())`.
  `scripts/app_ci.sh` finds these (in the workflow and in the suites' defaults) and gives each a database of its own.
  Reading a test variable with `std::env::var` in `tests/` is fine: the operator-settings check skips `tests/`.
- **A clean start.** Reset at the start of each suite, the vault's schema included:

```rust
sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;")
    .execute(&db.pool).await.expect("reset");
```

- **Never a database someone works in.** `scripts/app_ci.sh` refuses to reset a database named like `ferrum_dev`,
  `walk` or `ferrum_test`, and adds `_localci` to every name it makes.

### Booted as the server boots it

Register the app by its id, owning the packages it ships, with the vault open, and its siblings as the server registers
them ([docs/plan/stateless.md](../plan/stateless.md), *Registered as the server registers them*). From Campaigns'
`tests/tier1_it.rs`, which needs Comms, ERP and CRM beside it:

```rust
let db = Db::connect(DbConfig { url: url.clone(), max_connections: 20, ..Default::default() }).await?;
sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await?;
ferrum_kernel::vault::open(&db, opener(&url), false).await?;          // the credential slot, as a server opens it

let mut packages = ferrum_domain::packages()?;
packages.extend(ferrum_app_comms::packages()?);
packages.extend(ferrum_app_erp::packages()?);
packages.extend(ferrum_app_crm::packages()?);
packages.extend(ferrum_app_campaigns::packages()?);
let catalog = Catalog::from_packages(packages)?;

let mut hooks = HookRegistry::default();
ferrum_domain::register(&mut hooks);
hooks.app("comms", &[], ferrum_app_comms::register);
hooks.own("comms", &comms_packages);
hooks.app("erp", &[], ferrum_app_erp::register);
hooks.own("erp", &erp_packages);
ferrum_app_crm::register(&mut hooks);
let mut campaigns = None;
hooks.app("campaigns", &campaigns_packages, |h| {
    ferrum_app_campaigns::register(h);
    campaigns = Some(h.vault());                                       // the test keeps a handle to look in the vault
});
hooks.own("campaigns", &campaigns_packages);

let k = Kernel::boot(db, catalog, hooks, settings, None).await?;
ferrum_domain::maintenance(&k).await?;                                 // partitions
let tenant = k.create_tenant(NewTenant { slug: "acme".into(), /* … */ packages: vec!["core".into(), "comms".into(), "campaigns".into(), "selling".into()], .. }).await?;
```

- `hooks.app(id, …)` for each app whose code takes its vault handle or file kinds; outside it the kernel refuses to boot
  ([08](08-secrets-and-the-vault.md)).
- `hooks.own(id, &packages)` for each app whose secret fields are read: without it the fields are the platform's and the
  app's `Vault::field` refuses them.
- The `opener` is a `vault::Opener` with the database address (it must hold a password), a key file that is not there,
  and a cheap `vault::Kdf { m_kib: 64, t: 1, p: 1 }` ([08](08-secrets-and-the-vault.md), *Tests*).
- Give the kernel the old key only if the app still has R1 code that reads it (`KernelSettings { secret_key: … }`): the
  kernel takes it in and empties the field.
- Several replicas: run the same work at once and check it happened once. Metrics' `external_it` runs its upkeep four
  times at once, as replicas starting together would, and checks the environment's token was kept in the vault once.
  The kernel's own suites boot two kernels on one database where the difference matters (`companions_it`: two replicas
  rendering at once).

### What a scenario test covers

- The flow a person walks, through the kernel's record calls as a person's context, not through the app's tables.
- The refusals: a person without the right, another workspace's record (not found), a closed vault, a sandbox refusing to
  reach outside, an address the secret may not go to.
- What stays private: Metrics' `external_it` reads every row of every table, the vault's included, as text, and checks
  the token is nowhere in plain text.
- Twice: an event delivered twice, a job run twice, a backfill stopped half way.
- Upgrades: the previous release's data, read and worked on by this one.

## Tests that fail first

A test that never failed has not shown that it can. Write the test, run it, see it fail for the reason you expect, then
make it pass. For a fix, the test reproduces the bug first. Commit messages and the CHANGELOG say what was run.

## Mutants: proving the tests prove something

A green suite proves little if it stays green with the guarantee gone. A *mutant* removes one guarantee from the code
(swaps a check for `true`, drops a filter); its test must then fail. `scripts/mutants.py` is the platform's: each entry
is `(name, file, the text it finds, what it puts instead, the test to run)`, it restores every file it touches even when
interrupted, refuses to start on a dirty file, and fails when a mutant survives:

```python
# scripts/mutants.py
("file-kind-handle-anywhere", "crates/kernel/src/files.rs",
 "let ok = self.mint != 0 && k.hooks.vault_mint(&self.app) == Some(self.mint)",
 "let ok = true || self.mint != 0 && k.hooks.vault_mint(&self.app) == Some(self.mint)", FILE_KINDS),
```

```bash
scripts/mutants.py            # all of them, 10-20 minutes
scripts/mutants.py hook       # those whose name contains hook
scripts/mutants.py --check    # only that each still finds its text (CI does this on every push)
```

It runs only on the platform's files. An app keeps its own copy in its repository with its own mutants, as ERP
(`scripts/mutants.py`: the warehouse, the shop floor, quality, books) and Cloud do. Give each guarantee that protects
money, access or someone's data a mutant; run them before a release and whenever you change the code they touch.

## Real software, not stand-ins

Test against the real program the app talks to wherever it can run on a developer's machine, and say plainly what was
not run:

| App | The real thing in its tests |
|---|---|
| Comms | A real Stalwart mail server for hosted mail (`tests/stalwart.sh`, Docker), started in its workflow; without one the hosted-mail part prints "hosted mail skipped: no FERRUM_MAIL_URL (tests/stalwart.sh starts a mail server)" |
| Metrics | Its real engine, served in the test process on a real port (`tests/external_it.rs`) |
| The platform | LiveKit 1.13.11 started on a rendered file (`crates/kernel/tests/companions_it.rs`, where `livekit-server` is installed); a real Forgejo 17.0.1 (`scripts/oidc_rehearsal.sh`); real S3 servers (`scripts/object_store_rehearsal.sh`); PgBouncer in transaction mode (`scripts/managed_pg.sh`) |

How to say what never ran:

- A test that needs a program it cannot find **skips loudly**, one line naming what it needs and how to get it, and
  passes; it never pretends. Look for those lines in the output before saying a run was complete.
- The app's README and CHANGELOG say what was run against the real thing, with versions, and what was not
  ("Stalwart is not on this machine, nothing of it was run"). [20](20-writing-for-people.md) and
  [22](22-readme-and-repository.md) cover the wording.
- A fake (a fake tax authority, a test connection's `fixture`) is fine for the app's own logic; it is never evidence that
  the integration works.

## Checks run on the developer's machine

Checks run here, not on GitHub: an app's workflow file is kept as the description of its CI, and the platform's
`scripts/app_ci.sh` runs it on this machine. Commits say so (`[skip ci]`, "Checked on this machine with the platform's
scripts/app_ci.sh"), and carry no attribution lines for tools.

### `scripts/app_ci.sh`

```bash
scripts/app_ci.sh ../apps/ferrum-app-comms                 # from the platform's checkout
PLATFORM=… FRONTEND=… PG=postgres://ferrum:ferrum@localhost:5432 NO_DEBUG=1 scripts/app_ci.sh <app>
```

What it does, as the app's `.github/workflows/ci.yml` would on GitHub:

1. Builds the app against this platform checkout through a `.cargo/config.toml` patch it writes when there is none (it
   must be git-ignored, or it refuses), and puts back a committed `Cargo.lock` when it ends.
2. `cargo fmt --all -- --check`.
3. `cargo clippy --all-targets -- -D warnings` (with `--workspace` when the workflow tests a workspace).
4. `cargo test` with each test database made afresh (`<name>_localci`, owned by `ferrum`) and dropped at the end;
   `RUST_MIN_STACK` as the workflow sets it.
5. The interface (`ui/`): the committed `ui/dist` checked against its sources (`cli.mjs check`, by digest), then
   `typecheck` and `test`, with the Frontend's kit beside it ([17](17-the-interface.md)).
6. `== PASS: <app>` or `== FAIL: <app>`; exit 0 only when every step passed.

| Variable | Default | What it is |
|---|---|---|
| `PLATFORM` | this checkout | The Ferrum checkout to build against |
| `FRONTEND` | `<platform>/web` | A Ferrum-Frontend checkout with `node_modules` |
| `PG` | `postgres://ferrum:ferrum@localhost:5432` | PostgreSQL as the `ferrum` role |
| `NO_DEBUG` | unset | `1`: tests without debug info in a target of their own, deleted afterwards (a fraction of the disk) |

### `scripts/local_ci.sh`

The platform's CI on this machine, job by job (`rust`, `saml`, `audit`, `move`; `replica`, `managed`, `window`,
`objects`, `oneinput`, `oidc`, `companions` only when named). A job that needs software it cannot find says `SKIP` in
one line and passes. An app developer runs `audit` after changing what the platform compiles in (a pin), and the
rehearsal that matches what they changed.

## What each check script checks

| Script | What it checks | When an app meets it |
|---|---|---|
| `scripts/app_ci.sh` | An app's fmt, clippy, tests on real PostgreSQL, interface build and tests | Before every push of the app |
| `scripts/check_apps.py` | Every compiled app: manifest fields, licence, README named, CHANGELOG with the version, a scenario test, `ui/dist` built with `.source`, launcher icons the shell draws; operator settings sound and read through `ferrum_kernel::env::var` | When the platform pins the app |
| `scripts/secret_paths.py` | No app names the vault's tables, opens the vault or calls people's and operators' vault functions; no new cryptography crate or `unsafe`; no reading past the vault; the old key only where the baseline says | At the pin |
| `scripts/settings.py` | The settings' registry against what reads it; the apps' declarations merged, one owner each; the operations guide's table; the environment-only baseline only shrinks | At the pin |
| `scripts/licenses.py check` | Every Rust dependency's licence accepted; images deployed are approved engines | At the pin |
| `scripts/mutation_paths.py` | Every direct write to an `e_*` table in an app is listed and given a kind | At the pin |
| `scripts/mutants.py` | The platform's mutants each killed (`--check`: each still finds its text) | The platform's; an app keeps its own copy |
| `scripts/window.py` | The platform's own SQL and entities stay within the two-version window | The platform's only ([14](14-operations-for-apps.md)) |
| `scripts/registry.py check` | The requirements registry, accepted advisories, generated documents | At the pin, through `audit` |
| `scripts/check_manifests.py` | The deployment's connection budget and replica settings | When deployment files change |
| `scripts/one_input.sh` | The server with every pinned app on `DATABASE_URL` alone, two replicas, every variable read declared | When named (`oneinput`) |
| `server/src/packages.rs` tests (`compiled_apps_fit_together`) | What compiled apps require of each other is met; contributions meet their point's schema; icons; no two apps give one report name or share a route | In the platform's `rust` job, at the pin |

## Checked by

- `scripts/app_ci.sh` for the app; `scripts/check_apps.py` for the scenario test's presence.
- Whether a test failed first, whether the real software was run, and whether the README says what was not: reviewed,
  not checked.

## See also

- [16 Releasing](16-releasing.md), [24 Review checklist](24-review-checklist.md),
  [08 Secrets and the vault](08-secrets-and-the-vault.md), [02 Your first app](02-your-first-app.md),
  [17 The interface](17-the-interface.md)
- `scripts/app_ci.sh`, `scripts/local_ci.sh`, `scripts/mutants.py` (their headers say how to run them)
