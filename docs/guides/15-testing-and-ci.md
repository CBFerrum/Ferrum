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
- **The address.** Each suite reads its database from a variable of its own. With the test kit,
  `database("FERRUM_METRICS_TEST_DATABASE_URL")` is the variable's address, else `ferrum_metrics_test` on the local
  server. `scripts/app_ci.sh` finds these (in the workflow and in the suites) and gives each a database of its own.
  Reading a test variable with `std::env::var` in `tests/` is fine: the operator-settings check skips `tests/`.
- **A clean start.** The test kit empties the suite's database when it boots, the vault's schema included.
- **Never a database someone works in.** `scripts/app_ci.sh` refuses to reset a database named like `ferrum_dev`,
  `walk` or `ferrum_test`, and adds `_localci` to every name it makes.

### Booted as the server boots it

A suite boots from the test kit, `ferrum-testkit` under `[dev-dependencies]` ([docs/testkit.md](../testkit.md)), which
boots the kernel as the server does and in no other way. The kit's own suites are in the platform's repository
(`docs/testkit.md`); the suites of the app repositories boot the kernel themselves, as Comms' and Metrics' do. For a
suite that needs Comms, ERP and CRM beside Campaigns, on the kit:

```rust
use ferrum_testkit::{Kit, app, database, run};

#[test]
fn campaigns() {
    run(scenario());
}

async fn scenario() {
    let apps = [app!("comms", ferrum_app_comms), app!("erp", ferrum_app_erp), app!("crm", ferrum_app_crm), app!("campaigns", ferrum_app_campaigns)];
    let kit = Kit::boot(&database("FERRUM_CAMPAIGNS_TEST_DATABASE_URL"), &apps).await;
    let ws = kit.workspace("acme", &["core", "comms", "campaigns", "selling"], Value::Null).await;
    let campaigns = kit.vault("campaigns");                            // the app's own handle, to look in the vault
    // …
}
```

- Each app is registered as itself (`HookRegistry::app`), owning its packages: its hooks run for the workspaces with one
  of them (ERP's for every workspace), it takes its own vault handle, and `Vault::field` reads its secret fields
  ([08](08-secrets-and-the-vault.md)).
- The vault is opened before the kernel with its credential slot, from the address's password (it must hold one), with
  a cheap `vault::Kdf` ([08](08-secrets-and-the-vault.md), *Tests*).
- Several replicas: `kit.replica()` starts another server on the same database. Run the same work on both at once and
  check it happened once. Metrics' `services_it` renders its service's files on two replicas at once and checks the
  token was made once.
- Events: `ws.drain()` delivers the workspace's events and fails, naming them, when they do not go.

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
| The platform | LiveKit started on a rendered file (`crates/kernel/tests/companions_it.rs`, where `livekit-server` is installed); a real Forgejo (`scripts/oidc_rehearsal.sh`); real S3 servers (`scripts/object_store_rehearsal.sh`); PgBouncer in transaction mode (`scripts/managed_pg.sh`) |

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
`objects`, `oneinput`, `oidc`, `companions` only when named; `editors` needs Docker and Playwright and is not run here). A job that needs software it cannot find says `SKIP` in
one line and passes. An app developer runs `audit` after changing what the platform compiles in (a pin), and the
rehearsal that matches what they changed.

## What each check script checks

| Script | What it checks | When an app meets it |
|---|---|---|
| `scripts/app_ci.sh` | An app's fmt, clippy, tests on real PostgreSQL, interface build and tests | Before every push of the app |
| `scripts/check_apps.py` | Every compiled app: manifest fields, licence, README named, CHANGELOG with the version, a scenario test, `ui/dist` built with `.source`, launcher icons the shell draws; operator settings sound and read through `ferrum_kernel::env::var` | When the platform pins the app |
| `scripts/secret_paths.py` | No app names the vault's tables, opens the vault or calls people's and operators' vault functions; no new cryptography crate or `unsafe` | At the pin |
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
