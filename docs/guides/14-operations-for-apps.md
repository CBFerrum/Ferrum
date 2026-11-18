# Operations for apps

What an operator sees of an app, what an app must not do to a running installation, how it upgrades without stopping
anyone, and what it must write down for the people who run it. The operator's own guide is
[docs/operations.md](../operations.md); this one is the app developer's side of it.

## What an operator sees of an app

| Where | What it shows of an app | How the app puts it there |
|---|---|---|
| `ferrum task settings`, the Operations screen, the operations guide's table | Each operator setting as `<app>.<key>`, its value, where it comes from, the replicas still on an older value | `operator_settings` in `app.yaml` ([07](07-stateless-apps.md)) |
| `ferrum task services` | Each service the app declares: files, made secrets, why it cannot be rendered here | `HookRegistry::add_companion` ([10](10-companions-and-sign-in.md)) |
| `GET /api/operator/secrets`, `ferrum task vault_events` | The app's installation secrets, described (never a value), and every use and change | The app's vault handle ([08](08-secrets-and-the-vault.md)) |
| A workspace's Administration, *Secrets* | The app's secrets in that workspace; `managed_by` where the app manages one | The handle; `Vault::manage` |
| `ferrum task doctor` | Nothing app-specific | Nothing: an app adds no doctor line |
| The support report | Per workspace that installed the package: the app's counts and problems in words | `HookRegistry::add_health(package, Arc<dyn support::Health>)` |
| The log | The app's `tracing` lines, its upkeep failures and panics with its name, upkeep that took longer than its round | `tracing::warn!`, `error!`; the upkeep runner adds the app's name |
| `ferrum task deps_report` | Every entity a package names that another defines, and hooks that run on another package's records or in every workspace | The registrations themselves |
| `/metrics` | The platform's metrics. A compiled-in app adds none of its own; an app built into a server of its own (an extension, `ferrum_server::extensions::extend`) gives a `metrics` function | `Extension::metrics` (`server/src/extensions.rs`) |

### The support report: `Health`

```rust
// crates/kernel/src/support.rs
#[async_trait::async_trait]
pub trait Health: Send + Sync {
    async fn health(&self, kernel: &Kernel, tx: &mut Tx, tenant: Uuid) -> Result<Value>;
}
```

Asked only of workspaces that installed the package, in a transaction of their own. Answer counts and problems in
words: `{"problems": ["3 mailboxes have not synced for a day"], "mailboxes": 12, "failing": 3}`. Only numbers and the
`problems` list pass (20 problems at most, each cut at 200 characters), so a record never leaks through it. Say what
needs attention and how many, never whose.

### Alerts

The chart has no alert rules for the server ([docs/operations.md](../operations.md), *Object storage*). An app has none
either. What an operator alerts on is the platform's metrics and the log; an app helps by logging a failure once, at
`warn` or `error`, with what failed and what to do, and not again every second (a failing upkeep is logged each round:
make each round's line worth reading).

## Limits and performance

The platform's limits are the app's ([docs/limits.md](../limits.md), [docs/operations.md](../operations.md),
*Capacity*):

| Limit | Value | What it means for an app |
|---|---|---|
| Statement timeout | 15 s by default (`DbConfig::statement_timeout_ms`), set in every transaction the kernel opens, `Kernel::list` included; a report or an export reading page after page passes 60 s (`report_timeout_ms`) to `Kernel::list_with_timeout` | One statement over it is cancelled, and a request answers `503` (`unavailable`, "query took too long"). Do long work in batches |
| `lock_timeout` | 10 s | Waiting on a lock longer fails: keep transactions short, and lock rows in the same order everywhere |
| Idle in a transaction | 60 s | A transaction held open while waiting on the network is cut. Never call out inside one |
| Request time | `request_timeout_ms`, 2 minutes by default | A request is not a job: queue what takes longer ([06](06-events-jobs-and-logic.md)) |
| The pool | `FERRUM_DB_MAX_CONNECTIONS` (30) per replica, plus the side pool | Past it a request waits for a connection and answers 503. Do not hold two connections at once in one request |
| Upkeep | Every 60 s, one replica per app, `FERRUM_UPKEEP_CONCURRENCY` (2) apps at once per replica | A slow app delays no other, but a round longer than 60 s is logged. Keep a round bounded: budgets per step, as Comms does (`RETRY_BUDGET`, `POLL_BUDGET`, … in `src/lib.rs`) |
| A file read whole | 256 MiB (`files::MAX_READ_WHOLE`) | Stream larger ones ([09](09-files-and-storage.md)) |
| A site's request body | `Site::max_body`, 1 MiB by default, at most 1 GiB | Raise it per route only (`max_body_for`) |
| A job attempt | `JobHandler::timeout`, an hour by default | Make long work resumable (`Job::resumed`) |

**Batches.** Work over many rows goes in batches, each in its own transaction, resumable from where it stopped, with
`for update skip locked` where replicas share it: Cloud's `reseal` (`src/keys.rs`, 100 a batch), the kernel's
`tie_named_files` (1000 a batch), backfills (1000 a batch unless `batch` says otherwise).

**Indexes.** An entity's table gets the indexes its metadata implies from the schema sync, built concurrently so writes
carry on (a foreign key is added `not valid` and validated apart). Your own SQL runs in a transaction and cannot do that:
a plain `create index` or an `add constraint` that checks the rows as it is added holds writes on a table that has rows for as
long as it runs. `scripts/window.py` refuses both in a new file of the platform's own, and `scripts/app_ci.sh` runs it
(`--app`) over every file of your `packages/*/sql` after a package's first (the first runs against tables the sync has just made, which are empty); a statement that is safe all the same says `-- window: safe, <why>`. An app's own table gets the indexes its SQL makes: one for each way the app looks rows up, always starting
with `tenant_id`. Every query the app runs on its own tables names `tenant_id`; row-level security filters by it anyway,
and an index that does not lead with it makes that filter a scan.

**Measure.** The reference workload and targets are in [docs/operations.md](../operations.md), *Reference workload and
targets*. An app with heavy queries says in its README what it measured and on what data; one that measured nothing
says so.

## Upgrades

### The two-version window

A rollout runs the release before and the new one on the same database at once, and a rollback runs the release before
on the database the new one migrated ([docs/operations.md](../operations.md), *Rolling back and the two-version
window*). So every change an app makes to its tables is **additive** in the release that adds it:

| Allowed in one release | Not in the same release (needs two) |
|---|---|
| A new table; a new nullable column, or one with a default; a new index; a new function | Dropping or renaming a table, column, type or function; changing a column's type; making a column required; a new unique constraint on a table with rows |
| A new entity or field in metadata | Taking an entity or field out; changing a field's type or link target; making a field unique |

The second kind waits for the release after, once no build that reads the old shape can start. The platform checks its
own `packages/` with `scripts/window.py`; an app's two-version window is not checked by it (reviewed, not checked); only what holds writes is (`--app`, in `app_ci.sh`). Write
`-- window: safe, <why>` inside a statement you have reasoned is safe, as the platform's files do, so a reviewer sees why.

### Package SQL and its numbering

An app's own tables, functions and triggers are SQL files in `packages/<package>/sql/` (`crates/meta/src/catalog.rs`).
The schema sync applies each file once per installation, in file-name order, after the entity tables exist, each in a
transaction of its own on the direct connection, and records it with its SHA-256 in `cluster_package_sql`
(`crates/kernel/src/ddl.rs:633-660`).

- **Number them** so that name order is the order you mean: `001_cloud.sql`, `002_resources.sql`, … (Cloud's, up to
  `025_forge_sign_in.sql`). Three digits leave room.
- **Never edit a file once released.** A changed file stops the start: "package SQL <package>/<file> changed after it was
  applied; add a new file instead of editing it". Fix forward in a new file.
- **One file is one transaction.** It applies whole or not at all; no `CONCURRENTLY` inside it.
- **Idempotent where you can** (`create table if not exists`, `create or replace function`): an installation restored
  from a backup may have some of it already.
- A failing file stops the start, naming it: test every file on an empty database and on the previous release's.

### Data that moves: backfills

Data a release moves once its schema has grown (a new column filled from an old one) is a backfill in `package.yaml`
(`BackfillDef`: `name`, `table` or `entity`, `needs` (which rows still need it, as SQL over `t`), `set`, `batch`). It runs
per workspace as a job, a batch per transaction, resumed where it stopped, while the workspace keeps working; its report
says what it changed and what it could not (`crates/kernel/src/backfill.rs`). From Contracts:

```yaml
backfills:
  - {name: contract_not_confidential, entity: Contract, needs: "t.confidential is null", set: "confidential = false"}
```

Records made before a release still work after it: a release that adds a standard account, a field or a default
reaches them (a backfill, a default), or says how to ([docs/app-contract.md](../app-contract.md), *Reviewed*).

### Rolling restarts

Replicas restart one at a time (`maxUnavailable: 0`); a stopped replica finishes its requests in flight (up to 20 s),
hands its jobs back and closes its pools. So an app's code expects to be stopped at any `await`: a job's work is dropped
and its transaction rolls back, and the next attempt carries on ([06](06-events-jobs-and-logic.md)). A `start` setting
takes effect replica by replica: for a minute two values are live at once. A service's program restarts on its own
schedule: the vault's overlap covers the gap ([10](10-companions-and-sign-in.md)).

## Observability

- **Logs** are JSON on stdout (`log_level`). Use `tracing` with fields (`tracing::warn!(app = "x", error = %e, "…")`), one
  line per event worth an operator's attention. Never a secret, a token or a person's data in a line ([13](13-security-rules.md)).
- **Traces**: the server exports OpenTelemetry when the operator sets the `OTEL_*` settings ([docs/operations.md](../operations.md),
  *Monitoring*). Whether an app's own `tracing` spans appear in them was not checked for this guide.
- **The audit trail** records changes to records and secrets for you; an app records an action of its own that changes
  what people can do (a key revoked, an account locked) with `ferrum_kernel::audit::record` or `record_as`, as the
  kernel's companion clients are recorded (`crates/kernel/src/oidc.rs`), see [05](05-permissions-and-people.md).

## What an app must document for operators

In the app's repository (`docs/operations.md` or a section of the README, [22](22-readme-and-repository.md)), and briefly
in the platform's [docs/operations.md](../operations.md) where the app is pinned:

- Each operator setting: what it does, its default, whether it applies at once (the manifest's `doc` is the start).
- Each program it runs beside the server: what it is, its version, its service's files, how its secrets rotate and
  whether a rotation needs a restart within the overlap (Comms' `mail` does, at once).
- Each secret the operator gives it (`PUT /api/operator/secrets/<app>/<name>`): what it is for, its rule (`reveal`,
  `hosts`), and how to rotate it.
- What it keeps outside the database and the store (a database at a provider, a forge's repositories), and what an
  operator does for a move, a restore and a workspace's deletion (`Leaving`, [12](12-privacy-and-data-duties.md)).
- What its upkeep does each round, and what a failure in the log means.
- What it measured, and what it never ran against the real thing ([20](20-writing-for-people.md)).

## Checked by

- `HookRegistry::settle` and the schema sync at every start (declarations; package SQL changed after it was applied).
- `scripts/settings.py`: the operations guide's settings table, generated from the registry with the apps' declarations.
- `scripts/check_manifests.py`: the deployment's connection budget.
- `scripts/window.py --app`: a plain `create index` or a row-checking `add constraint` in an app's package SQL after a package's first file, on a table the file does not make. The two-version window of an app's SQL and metadata changes: reviewed, not checked.
- What an app documents for operators: reviewed, not checked.

## See also

- [07 Stateless apps](07-stateless-apps.md), [06 Events, jobs and logic](06-events-jobs-and-logic.md),
  [10 Companions and sign-in](10-companions-and-sign-in.md), [16 Releasing](16-releasing.md),
  [04 Data and records](04-data-and-records.md)
- [docs/operations.md](../operations.md), *Upgrades*, *Capacity*, *Monitoring*; [docs/limits.md](../limits.md)
