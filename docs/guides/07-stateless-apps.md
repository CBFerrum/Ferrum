# Stateless apps

A Ferrum replica starts from one input, the database's address, and keeps everything in two stores: PostgreSQL and
the object store. An app compiled into the server runs inside that replica, so it lives by the same rules. This guide
says what that means for an app's code, and how to follow each rule. The decisions and their reasons are in
[docs/plan/stateless.md](../plan/stateless.md); the operator's side is in [docs/operations.md](../operations.md),
*Configuration* and *Settings in the database*.

## The rules

From [docs/plan/stateless.md](../plan/stateless.md), *Rules that every part keeps*, as they bind an app:

| Rule | What it means for an app |
|---|---|
| One input | A replica starts from `DATABASE_URL` alone. An app reads no variable of its own: what the operator chooses is an *operator setting*, read through `ferrum_kernel::env::var` (a variable that is set wins over the database's value) |
| Two stores | PostgreSQL and the object store. An app keeps nothing else it cannot rebuild: no file on local disk, no cache that is the only copy |
| Secrets are used, not handed out | A secret lives in the app's own vault ([08](08-secrets-and-the-vault.md)), never in a setting, a variable or a file the app writes |
| No superuser, no session state | The app's SQL works as an ordinary role, through a pooler in transaction mode, on managed PostgreSQL |
| Additive | A release adds tables and columns; nothing the release before reads changes shape ([14](14-operations-for-apps.md)) |
| Fail closed, say why | An app that cannot reach what it needs refuses, in plain words, and never falls back to a weaker mode quietly |

Where each thing an app has belongs:

| What | Where it lives | Guide |
|---|---|---|
| A value the operator chooses for the whole installation (an address, a limit) | An operator setting declared in `app.yaml`, kept in the `setting` table as `<app id>.<key>` | below |
| A value each workspace's administrators choose | The app's settings page (`settings:` in the manifest) | [03](03-the-manifest.md) |
| A password, a token, a key | The app's vault, under a rule | [08](08-secrets-and-the-vault.md) |
| Bytes (attachments, backups, artifacts) | The object store, through the platform's file seam | [09](09-files-and-storage.md) |
| Records and the app's own tables | PostgreSQL, with `tenant_id` and row-level security | [04](04-data-and-records.md) |
| A program's configuration file (LiveKit's, a mail server's) | Rendered at the program's start from the database and the vault | [10](10-companions-and-sign-in.md) |
| Something only one replica remembers | Nowhere. A process-local value is only ever a cache of what the database says | below |

## Operator settings

An operator setting is a value the operator chooses for the whole installation and the app's code in the server reads:
an address of a service, a size limit, a switch. The contract is [docs/app-contract.md](../app-contract.md),
*Operator settings*. It is not the settings page (`settings:` in the manifest), which each workspace's administrators
fill, and it is never a secret.

### Declaring one

Each entry of `operator_settings` in `app.yaml` (the fields are `ferrum_apps::OperatorSetting`,
`crates/apps/src/manifest.rs`):

| Field | Required | What it is |
|---|---|---|
| `key` | yes | snake_case; kept as `<app id>.<key>`, at most 63 characters together |
| `env` | yes | The environment variable that gives the setting too, and wins over the table's value: an upper-case letter, then upper-case letters, digits and `_`, 3 to 100 characters; never `DATABASE_URL` |
| `type` | no | `text` (the default), `bool`, `int` (a whole number, 0 or more), `json` or `choice` |
| `values` | for `choice` | The choices, each once (`""` among them when it may be left empty); only a `choice` has them |
| `default` | no | As the variable would say it; `""` for the app's own default. Must fit the type |
| `applies` | no | `start` (the default): at a replica's next start; `live`: every replica takes a change within seconds |
| `sends` | no | The variable of a secret this address is given (below). Not one of the app's own declared variables |
| `doc` | yes | What it does, for the operations guide: at most 1000 characters |

A real one, from Drive (`ferrum-app-drive/app.yaml`):

```yaml
platform: ">=0.3.9, <0.4"        # the platform that reads operator settings
operator_settings:
  - key: max_file_mb
    env: FERRUM_DRIVE_MAX_FILE_MB
    type: int
    default: "32"
    applies: live
    doc: >-
      Largest file in one request to Drive, in MB (an upload, a new version or an editor save), at most 1024. A replica
      holds each such file in memory while storing it. Raise any proxy's body limit in front with it.
```

Rules the parser and the checks hold you to:

- **Only a native app** has operator settings: the manifest is refused for any other runtime ("operator_settings: only
  an app compiled into the server reads them", `validate_operator_settings` in `crates/apps/src/manifest.rs`).
- **The platform that reads them.** `scripts/check_apps.py` fails a manifest with `operator_settings` whose `platform`
  range does not start at 0.3.9 or later.
- **No other field.** `scripts/check_apps.py` refuses any field outside the eight above, saying that a secret is the
  app's vault's. There is no field for a secret, on purpose.
- **One owner each.** The server adds every compiled-in app's declarations to its registry before it reads any setting
  (`merged` in `server/src/settings.rs`). A key or variable two owners declare stops the start, naming both: "X is
  declared twice: by the app A (a.k) and by the app B (b.k)". A variable the platform holds as a secret is refused: "it
  is a secret, and an app keeps its secrets in its own vault". An entry of the platform's registry marked `app:` in
  `server/src/settings.yaml` is replaced by that app's declaration of the same variable, without a clash.

### Reading one

Read it with `ferrum_kernel::env::var(<the variable>)`, never `std::env::var`, at each use:

```rust
// Metrics (ferrum-app-metrics/src/engine.rs): the engine's address, at each use, so a change applies at once.
pub(crate) fn address() -> Option<String> {
    ferrum_kernel::env::var("FERRUM_METRICS_ENGINE_URL").map(|u| u.trim().to_string()).filter(|u| !u.is_empty())
}
```

What `ferrum_kernel::env::var` gives (`crates/kernel/src/env.rs`; `Snapshot::resolve` in `server/src/settings.rs`):

1. The environment's value, when the environment sets the variable at all, even to nothing.
2. Else the `setting` table's value.
3. Else `None`. The manifest's `default` is **not** filled in: `None` means "nothing sets it", and the app applies its
   own default. Keep the code's default and the manifest's `default` the same; the manifest's is what the operations
   guide shows.
4. For a name the server's registry does not know, the environment itself.

Treat an empty value as unset (`.filter(|v| !v.trim().is_empty())`): a variable set to nothing reaches you as
`Some("")`. Cloud wraps this once (`ferrum-app-cloud/src/operator.rs`, `setting()`); so can you.

**`live` or `start`.** For a `live` setting, `env::var` gives what the table says now: each replica rebuilds its snapshot
within seconds of a change (the channel `ferrum_settings`, and a comparison every 30 seconds). For a `start` setting it
gives the value the replica started with until it starts again (`Snapshot::vars` in `server/src/settings.rs`), and
`ferrum task settings` lists the replicas still running the old value. Choose `live` when your code reads the value at
each use and nothing was built from it at start; choose `start` when a value shapes something made once (a pool, a
client built at start). Since `env::var` is read at each use, most apps' settings are `live` (every setting Drive, Comms
and Metrics declare is).

`scripts/check_apps.py` fails an app that declares a variable and reads it with `std::env::var` (or `env::var_os`) on a
line that does not say `ferrum_kernel::env::var`: "reads X with std::env::var: read it with ferrum_kernel::env::var, or
the database's value never reaches it". Tests, examples, benches and `build.rs` are not looked at.

### An address that is given a secret

When the app sends a secret to an address the operator sets (LiveKit's server, a mail server, an engine), the address
says so with `sends:` naming the secret's variable. While that secret is in the environment, the table's value of the
address is not used: a row someone writes into the database never sends the environment's secret to their own host
(`given_secret` in `Snapshot::resolve`). Cloud (`ferrum-app-cloud/app.yaml`):

```yaml
  - {key: forge_url, env: FERRUM_CLOUD_FORGE_URL, sends: FERRUM_CLOUD_FORGE_TOKEN, doc: "The forge's API as the controller reaches it, http://ferrum-forge:3000. Its token is Cloud's secret forge.token, bound to this host, or the controller's own, made with the administrator's password at /run/forge/admin_password."}
```

A secret kept in the app's own vault is bound to its hosts by its rule (`Rule.hosts`), so moving the address does not
move the secret with it ([08](08-secrets-and-the-vault.md), *Hosts and carry*).

### What the operator does with them

`ferrum task settings` lists every setting with where its value comes from; `ferrum task set key:drive.max_file_mb
value:64` (or `key:FERRUM_DRIVE_MAX_FILE_MB`), `ferrum task unset`, `ferrum task apply_settings file:<document>`, and
the Operations screen change them, each checked by the declared type and recorded in `setting_change`
([docs/operations.md](../operations.md), *Settings in the database*). The operations guide's table lists the app's
settings under the app, generated by `python3 scripts/settings.py --write`.

## Nothing on local disk

A replica's disk is gone when its container is. So:

- **Files** go to the object store through the platform's seam (`Kernel::store_file`, a `FileKind` for the app's own
  operational files): [09](09-files-and-storage.md). An app holds no store, bucket or keys of its own (Cloud's files:
  `ferrum-app-cloud/src/files.rs`).
- **Scratch** is fine where it can be rebuilt and nothing else reads it: Cloud's build scratch and serving snapshots
  stay on their volumes for that reason ([docs/plan/stateless.md](../plan/stateless.md), *D6, other programs'
  stores*). Say in the app's operations notes what is scratch and what happens when it is lost.
- **A program beside the server** takes its configuration from files rendered as it starts, from the database and the
  vault: [10](10-companions-and-sign-in.md). Never a file an operator types once for the program and once for the server.
- **A path on disk in a setting** is a smell: a path names one machine. The platform's own exceptions are fixed paths
  under `/run/ferrum` that only the deployment mounts (the vault's key file, the database's certificate authority).

There is no check that an app writes nothing to disk: reviewed, not checked. `scripts/one_input.sh` runs the real server
with every pinned app on a fresh database with `DATABASE_URL` alone, as two replicas, and fails on any variable read
that the registry or the baseline does not declare.

## Any PostgreSQL

The server runs on its chart's PostgreSQL or on a managed one (RDS, Aurora, Cloud SQL, Azure, Supabase, Neon or your
own: [docs/operations.md](../operations.md), *Any PostgreSQL*). It connects as an ordinary role that owns its database,
and refuses to start as a superuser or a role with `BYPASSRLS`. An app's SQL, its package SQL included, runs as that role.

**What the role can do, and so what an app may use.**

- The extensions are `btree_gist`, `citext` and `pg_trgm`, which a database's owner may make. An app needs no other:
  a managed service may not offer it.
- No superuser-only statement, no `ALTER SYSTEM`, no grant on `session_replication_role` (moves, sandboxes and deletions
  run without it; [docs/plan/stateless.md](../plan/stateless.md), D7).
- Row-level security is forced on tenant tables, and the role has no way past it. That is the point: tenant isolation
  holds even if an app's query forgets a `where tenant_id = …`. See [04](04-data-and-records.md) and
  [13](13-security-rules.md).

**A pooler in transaction mode.** With PgBouncer (or RDS Proxy, Supavisor, Neon's pooled address) in front,
`DATABASE_URL` is the pooler, and each transaction may get a different server connection. The platform sends what
needs a session (its listeners, session advisory locks, migrations, the schema sync) to PostgreSQL directly
([docs/operations.md](../operations.md), *Connection pooler*); an app's code only ever gets pooled transactions.
So, in an app's code:

| Do not | Because | Do instead |
|---|---|---|
| `SET …` or `set_config(…, false)` | A session setting stays on a server connection another client gets next | `set_config(name, value, true)` or `SET LOCAL`, inside the transaction. The kernel's own transactions set the workspace and the statement timeout this way (`tenant_tx_with_timeout` in `crates/kernel/src/db.rs`) |
| `pg_advisory_lock` / `pg_try_advisory_lock` (session locks) on Ferrum's database | The lock stays on a pooled connection, held for whoever gets it next; a test through PgBouncer saw a start wait on one for good | `pg_advisory_xact_lock(hashtextextended('<app>:…', 0))` in the transaction that needs it (Commerce's `src/checkout.rs`; the kernel's `oidc.rs`), or `select … for update skip locked` on a row |
| `LISTEN` | It hears nothing through a pooler | Poll a table, or use the platform's events ([06](06-events-jobs-and-logic.md)). `pg_notify` inside a transaction is fine |
| A temporary table, a cursor or a prepared statement you name, kept across transactions | Gone, or on someone else's connection | Keep it within one transaction |
| `CREATE INDEX CONCURRENTLY` in package SQL | Package SQL runs inside a transaction (`crates/kernel/src/ddl.rs`, pass 3), where `CONCURRENTLY` is refused | A plain `create index` in a new file; mind its lock on a large table ([14](14-operations-for-apps.md)) |

An app's upkeep needs no lock of its own: the platform hands each workspace's run to one replica at a time through a
row it claims with a lease (`upkeep`, `server/src/packages.rs`), so your `maintenance(k, tenant)` takes none.

A connection an app opens to *another* database (Cloud to a customer's database, or to the forge's) is that app's
business; Cloud's session lock there (`ferrum-app-cloud/src/resources/migrate.rs`) does not touch Ferrum's.

**Timeouts the kernel sets.** Every transaction it opens sets a statement timeout (15 s by default,
`DbConfig::statement_timeout_ms`), a `lock_timeout` of 10 s and an `idle_in_transaction_session_timeout` of 60 s in its
first statement ([docs/operations.md](../operations.md), *Connection pooler*). Work that needs longer is a job in
batches, not one long statement: [14](14-operations-for-apps.md).

## Several replicas at once

Any replica may serve any request, run any job and die at any time ([docs/operations.md](../operations.md),
*Components*). Write every piece of an app as if another copy of it runs beside it, because one does.

**Upkeep.** The `maintenance` function an app gives runs every 60 seconds on one replica at a time for that app: each
replica tries the app's lock and skips it when another holds it (`round` in `server/src/packages.rs`). It must be
idempotent and cheap (Comms says so of its own, `src/lib.rs`). A failure or a panic is logged with the app's name and
stops nothing; a round longer than the interval is logged too. Upkeep for a sandbox's turn reaches nothing outside
([12](12-privacy-and-data-duties.md)).

**Jobs and events are at least once.** A job runs at least once and must be safe to repeat (`JobHandler::run`,
`crates/kernel/src/jobs.rs`); a worker holds a lease it renews, and another takes the job when it lapses. An event
sink may see an event twice (`EventSink`, `crates/kernel/src/hooks.rs`). There is no exactly-once delivery; there is
an exactly-once *effect*, made by the app:

- a `dedupe_key` on `NewJob`, so that a job already queued or running is returned instead of a second one;
- a unique key in the app's table for what may only happen once, and `insert … on conflict do nothing`;
- a claim on a row: `update … set claimed_by = … where … for update skip locked`, as Cloud's upkeep applies each of its
  forge holds once however many controllers run (`ferrum-app-cloud/src/code/sign_in.rs`);
- the vault's own once-only verbs: `Vault::adopt` writes a value only where there is none, under the secret's lock, so
  replicas starting together write it once ([08](08-secrets-and-the-vault.md)).

[06](06-events-jobs-and-logic.md) covers events, jobs, sagas and idempotency in full.

**Process-local state.** A `static` is per replica and lost at restart. Use one only for:

- what the app's registration gave it (its vault handle, its file kinds). Keep these keyed by the registry that gave
  them, since one process may hold several kernels (a test's replicas, the two set-ups of `ferrum start`): Comms keys
  its handles by the address of the site it registered (`ferrum-app-comms/src/keys.rs`, `HANDLES`), Cloud its file kinds
  likewise (`ferrum-app-cloud/src/files.rs`, `KINDS`). Metrics keeps the last registration's handle for the whole
  process and says so in a `ponytail:` comment (`ferrum-app-metrics/src/keys.rs`);
- an optimisation the database would answer the same way.

Never keep in memory the only record that something happened, a counter that must hold across replicas, or a
per-person limit: use the database (rate limits: `Kernel::rate_limit`, [13](13-security-rules.md)).

## Readiness

A replica answers `/api/ready` with 503, saying why, until its database answers and its object store has taken a write
and a read since it started ([docs/operations.md](../operations.md), *Object storage*, Readiness). An app has no
readiness hook of its own: there is none in `HookRegistry` (`crates/kernel/src/hooks.rs`). So:

- An app whose outside service is down (LiveKit, a mail server, an engine) still lets the replica serve, and refuses
  each use of that service in plain words: Metrics answers "Metrics' engine cannot be reached" and logs why
  (`src/engine.rs`); Comms answers "video meetings are not set up on this server" while `comms.meet_url` is unset
  (`src/meet.rs`).
- A closed vault is the same: every verb refuses with `Unavailable`, and the app turns that into a sentence a person can
  read, the cause going to the log (Comms' `unavailable`, Metrics' `refused` in their `keys.rs`).
- How an app's part of a workspace is doing goes in the support report (`HookRegistry::add_health`):
  [14](14-operations-for-apps.md).

## Checked by

- `crates/apps/src/manifest.rs` (`validate_operator_settings`): the shape of each operator setting, at every parse.
- `scripts/check_apps.py`: operator settings native only, on platform 0.3.9 or later, no field but the eight, and none
  of the declared variables read with `std::env::var`.
- `scripts/settings.py`: the apps' declarations merged into the registry, no key or variable with two owners, the
  operations guide's table generated; the baseline of what stays in the environment only shrinks.
- `server/src/settings.rs` (`merged`, `clash`): the server does not start on a clash or an app claiming a platform secret.
- `scripts/one_input.sh` (`scripts/local_ci.sh oneinput`, run when named): the server with its apps on `DATABASE_URL`
  alone, two replicas, a live change on both.
- `scripts/managed_pg.sh` and `scripts/local_ci.sh managed` (run when named): the platform's suites through PgBouncer in
  transaction mode as an ordinary role. An app's own suite is not run there; nothing checks an app's SQL for session
  state: reviewed, not checked.
- Writing nothing to local disk, and process-local state: reviewed, not checked.

## See also

- [08 Secrets and the vault](08-secrets-and-the-vault.md), [09 Files and storage](09-files-and-storage.md),
  [10 Companions and sign-in](10-companions-and-sign-in.md), [14 Operations for apps](14-operations-for-apps.md)
- [03 The manifest](03-the-manifest.md), [06 Events, jobs and logic](06-events-jobs-and-logic.md)
- [docs/app-contract.md](../app-contract.md), *Operator settings*; [docs/plan/stateless.md](../plan/stateless.md);
  [docs/operations.md](../operations.md), *Configuration* and *Any PostgreSQL*
