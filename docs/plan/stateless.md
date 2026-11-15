# One input, two stores: settings in the database, the vault, object storage, any PostgreSQL, Git

Status: being built (started 15 November 2026). The plan and its reasons: `STATELESS-VAULT-GIT-PLAN-2026-11-15.md`
(kept outside the repository). This file is the decision record; where the two differ, this file wins. It becomes
`docs/plan/stateless.md` in the repository with the first slice that lands.

## Words

- **The boot contract**: what a program may take from its environment.
- **Setting**: a value an operator chooses for the installation or for one pool. Not a secret.
- **Secret**: anything whose disclosure harms. Lives in the vault, never in a setting.
- **Owner**: who a secret belongs to: `platform`, an app's id, or `plugin:<name>`.
- **Slot**: one way of opening the installation key.
- **R1 / R2**: the release that adds a thing, and the release after it. The previous build must still start on the
  newer database (the two-version window), so anything an older build reads stays readable through R1.

## Rules that every part keeps

1. **One input.** After R2 a program reads `DATABASE_URL` and nothing else from its environment. Through R1, every
   variable that works today still works and still wins, so a rollback behaves.
2. **Two stores.** PostgreSQL and the object store. No program keeps anything else that it cannot rebuild.
3. **Secrets are used, not handed out.** Raw values leave the vault only where a secret's rule allows `reveal`.
4. **An owner opens only its own.** No public function takes an owner as a parameter from an app.
5. **A dump opens nothing.** No key that opens a sealed value is stored unwrapped in the database or the bucket.
6. **No superuser, no session state.** Everything works as an ordinary role, through a transaction-mode pooler
   (transaction-local settings only), on managed PostgreSQL.
7. **Additive.** New tables and columns only; nothing an older build reads changes shape in R1.
8. **Fail closed, say why.** A program that cannot open its root, read its settings or reach a store refuses to
   serve and names the cause in plain words. It never falls back to a weaker mode silently.
9. **Plumbing in four places** still holds for anything that must remain in the environment in R1 (see RULES).

## D1. The boot contract

- `DATABASE_URL` (required). Its `application_name` parameter names the pool: `pool:<name>`; absent means `default`.
- Files at fixed paths, read when present, never required: `/run/ferrum/root.key` (the file slot, 32 bytes as
  base64), `/run/ferrum/db-ca.pem` (a certificate authority to trust for the database).
- Start order: connect with a pool of two; read the pool's row; open the root (D3); read settings (D2); open the real
  pools; serve; listen for changes.
- Everything that today must be known before the database is reachable keeps a compiled default (log level and
  format, telemetry off) and is then replaced by the database's value.
- Port and bind address have fixed defaults (the container's); they remain overridable in R1 like everything else.

## D2. Settings

- **Table** `setting (scope, scope_name, key, value jsonb, version, updated_at, updated_by)`; `scope` is
  `installation` or `pool`; primary key `(scope, scope_name, key)`. Created by one early, never-reshaped migration.
- **Registry**: every setting is declared once in code: key, the legacy environment name, type, default, scope,
  whether it applies `live` or at `start`, whether it is a secret. A secret is declared there but never stored in
  `setting`: until the vault holds it (D4) it stays where it is today.
- **Precedence in R1**: environment (when set) over database over default. The doctor lists every setting that the
  environment decides. In R2 the environment is not read.
- **Adopt**: at start, a setting found in the environment and absent from the table is written to it once, as
  `updated_by = 'environment'`, under an advisory lock (two replicas starting together write it once).
- **Live apply**: a change notifies channel `ferrum_settings`; each replica rebuilds its snapshot and swaps it in
  atomically. A periodic compare of the table's highest version heals a missed notice within 30 seconds. A setting
  marked `start` takes effect at the next start; until then the replica is listed as running the old value.
- **Replicas table** `replica (id, pool, build_date, started_at, seen_at, settings_version)`, refreshed every 30
  seconds and aged out after two minutes. It answers "which replicas still run the old value" and "how many replicas
  are live" (per-user and per-address limits divide by that number).
- **Changing settings**: `ferrum task settings` (list, with where each value comes from), `set`, `unset`,
  `apply_settings file:` (a whole document, declarative; this is what the chart's Job runs), and operator routes for
  the Operations interface. Each change is validated by the registry's type, recorded on the operator's audit trail
  with old and new value, and never accepts a secret.
- **The chart**: values gain a `settings:` document and a Job that applies it (as the migrate Job runs). In R1 the
  default render is unchanged; `environment: minimal` renders only the database address (with the pool in
  `application_name`) and relies on the Job.
- **A check** (`scripts/settings.py`): every `get_env(` in the server's configuration and every read of the
  environment in `server/` and `crates/` is declared in the registry or is on a short allow-list (the boot contract,
  tests, build scripts); the operations guide's table is generated from the registry.

## D3. The vault: keys and slots

- **Sealing stays in the server**: AES-256-GCM (as `secret.rs` does today) with associated data that binds a value
  to its place. Nothing is encrypted by SQL functions.
- **Hierarchy**: slot → installation key → workspace key (one per workspace; one more for installation-wide
  secrets) → owner key (one per owner in that workspace) → secret. Every key below a slot is random, stored wrapped,
  and carries a version.
- **Slots** (`vault.root_slot`): the installation key wrapped once per slot; any slot opens it.
  - `credential`: wrapping key derived from the database password in `DATABASE_URL` with Argon2id and a per-slot
    salt. The default when nothing else is present. Refused when the address has no password.
  - `file`: the key in `/run/ferrum/root.key`.
  - `kms`: the existing Transit and command wrappers (`kms.rs`); its address and options are settings.
  - `recovery`: a key printed once by a task, for the owner to keep offline.
  - Opening order at start: `file`, then `kms`, then `credential`. A fresh database gets an installation key and the
    best slot available, made under an advisory lock so that replicas starting together make one.
  - Tasks add and remove slots, re-wrap for a changed database password (`old password` given once), print a
    recovery key, and show status. The last slot is never removable.
- **Tables** live in schema `vault`, with row-level security forced and a policy that requires the transaction-local
  setting `ferrum.vault = 'on'`, which only the vault module sets. A query that does not go through the module sees
  no row.
- **Key material in memory** is zeroed on drop and never formatted, logged or serialised.
- **Rotation**: a slot (one row); the installation key (re-wrap workspace keys); a workspace or owner key (new
  version; values re-sealed in the background; old version kept until none remain); one secret (versions; for
  secrets that verify, the previous version stays valid for its rule's overlap).

## D4. The vault: owners, verbs, rules

- **A handle bound to its owner.** `Vault` values are made only inside the kernel crate; an app receives one for its
  own id where the kernel calls it. There is no public function that takes an owner and returns secrets.
- **Verbs**: `set` / `rotate` / `remove` / `describe` (metadata only); `reveal` (raw value, only if the secret's rule
  allows it); `derive(purpose)` (a key from the owner key, nothing stored); `sign` / `verify` (HMAC and Ed25519);
  `seal` / `open` (data under the owner key); and, where the platform makes the call, credentials added to outbound
  requests for the hosts a secret names (the existing connections and plugin-host paths move onto this in D5).
- **Rule on each secret**: `reveal` yes/no, the hosts it may be sent to, how a sandbox treats it (`blank`,
  `regenerate`, `keep`), overlap on rotation, expiry.
- **People**: setting, rotating and removing need a right for that owner and a confirmed identity (the existing
  step-up). No route returns a value; one shown at creation is shown once.
- **Audit**: every `reveal`, `sign`, outbound use, set, rotate and remove is on the audit trail (owner, name, verb,
  workspace; never a value), under the existing cap on floods; `last_used` is kept coarsely on the secret.
- **A check** (`scripts/secret_paths.py`): no crate outside the kernel's vault module names the master key, the
  vault's tables or raw sealing functions; app crates use no `unsafe` and no cryptography crates directly.

## D5. Moving what exists into the vault (after D3 and D4 have landed)

- R1: the kernel's stores (secret fields, connections, plugin credentials, single sign-on secrets, SAML keys,
  two-factor seeds, webhook secrets, the push key) and the environment's secrets (session signing, bootstrap and
  metrics tokens, mail, object store, assistant, audit sink) are written to the vault; the old stores stay readable
  for the previous build. `settings.secret_key` stops being public. Session tokens are signed with a key pair that
  has a key id; the old shared secret still verifies until its tokens expire.
- R2: the old stores and the environment's secrets are dropped; `COMPATIBLE_FROM` moves.
- Apps: sealed columns of their own become vault secrets of that app; unsealed per-workspace keys become derived
  keys; country add-ons' credentials become connections.

## D6. Object storage only

The local directory is refused outside development and tests. Docker Compose bundles an S3-compatible store. The
store's address is a setting; its keys are a platform secret, or none where the pod has an identity. The doctor
checks versioning, lifecycle and encryption at rest. Objects are backed up beside the database, and a drill restores
both to one moment. Downloads stay through the server; short-lived direct links are a setting, never for entities
marked for recorded reads.

## D7. Any PostgreSQL

The chart takes an outside database (address, certificate authority, an egress rule). Direct and read addresses are
settings holding a host; the credential is the one in `DATABASE_URL`. A full dump works as the ordinary role. The
"same database" check uses a lock. A restore is noticed through a marker in the object store. Moves and sandboxes
run with an ordinary role. A rehearsal behaves like a managed service on this machine.

Decided while building it (15 November 2026):

- **The chart.** `database.external`: `urlSecret` (the older `database.urlSecret` still read), `directHost`, `readHost`,
  `caSecret` (a projected volume at `/run/ferrum`, so D3's `root.key` can join it there), `egress` (`cidrs`, `ports`).
  With an outside database the chart runs no Cluster, backups, pooler, read service or bootstrap; the migrate and
  settings Jobs get the direct host and the certificate authority, never the read host.
- **Hosts.** `database_direct_host` and `database_read_host` are stored settings (`host`, `host:port`; 5432 without a
  port), applied at start; the address is `DATABASE_URL` with its host replaced and any `host`, `hostaddr` or `port`
  in its query dropped. The old full addresses still win. A row in `setting` can therefore choose where the server
  sends `DATABASE_URL`'s credential, which D2's rule for addresses given a secret keeps every other secret from;
  with the certificate authority and `verify-full` a host it did not certify gets nothing (TLS fails before
  authentication). The two settings stay in the environment in `minimal`, as the migrate Job needs the direct
  host before the settings Job has run.
- **The certificate authority.** Read by the `ferrum` command before anything else: the three database addresses in
  the environment get `sslrootcert`, and a mode that checks it (none or `prefer` becomes `verify-full`, `require`
  becomes `verify-ca` as libpq does; an address's own `sslrootcert` is left alone; `disable` or `allow` refuses to
  start). One connection is tried first, so that a certificate that does not verify refuses the start in those words.
- **The dump.** `deploy/dump.sh`: pg_dump as the server's role with `ferrum.system` and `ferrum.vault` set for its
  session and `--enable-row-security`; the restore into an empty database in one transaction, without owners or
  grants, leaving out schemas the database has and comments on extensions. The chart's `import` mode passes the same
  setting as the `options` connection parameter and `--enable-row-security` as `pgDumpExtraOptions`.
- **"Is this the same database?"** A transaction lock (a random key) taken on the live database and tried through the
  copy's connection: only the same database refuses it, whatever the address or pooler. The identity stays as a second
  opinion only for a standby in recovery, streaming, on the live timeline (the lock does not see it, and no genuine
  copy is one): proven on a real streaming standby, refused while it follows and taken once promoted with the same
  identity. Used by extraction, `move_workspace` and `copy_workspace from:`. A logical replica of the live database is
  not seen by either: it is taken as a copy.
- **The restore marker.** `platform_incarnation.restore_marker` (NULL: from before the marker; 0: born with it, so it
  adopts whatever the store has) and `restore_marker_at`; in the store `restore-marker/<system identifier>-<database
  oid>.json`, keyed by identity so that a snapshot, clone or branch reads its original's counter and two databases
  sharing a store never meet. Every replica looks as it starts (after the store is set, before it serves) and every
  minute; the store is read first, then the database under its row's lock; behind the store, or behind what the
  process saw itself, is held as a restore is today (same columns, same setting aside). One replica moves it on at
  most every 30 seconds, database first. A fenced or held database moves nothing. A replica whose first look cannot
  reach the store sends nothing (`holding()`) until a look does; one that has compared carries on through an outage.
  Guests (an older build on a newer database) do not look. Not noticed: database and store taken back together; a
  restore to less than about a minute before the store's last write; a database given a store without its marker.
- **Copies as the ordinary role.** Every foreign key in `public` is DEFERRABLE INITIALLY IMMEDIATE, and every trigger
  there has `current_setting('ferrum.copy', true) IS DISTINCT FROM 'on'` in its WHEN (with its own WHEN, if any): a
  migrating start makes them so after the sync, a table at a time, the kernel's, the sync's and packages' alike
  (`ddl::copy_ready`; idempotent; what it cannot change is logged, never fatal), and the sync makes new ones so. A
  copy's transaction sets `ferrum.copy` and defers every constraint; its deletions run as one statement (a
  data-modifying CTE a table), so the ON DELETE RESTRICT links between entity tables, which form circles (a company and
  its letter head), are checked when both rows are gone. A copy refuses, naming them, while any key or trigger is not
  ready, and taking a workspace back refuses when a kept table refers to one going back with a rule that acts by itself
  (cascade, set null or default, restrict), which turning triggers off used to hide. Nobody else sees a difference
  (checked at once, triggers as before), and the build before still copies with `session_replication_role` and its
  grant. Ceiling: each copied row queues one check per foreign key in the copying session's memory until the commit
  (some bytes each): tens of millions of rows take hundreds of MB there; the way past it is copying in dependency
  order with checks at once, deferring only inside circles.
- **Failovers.** `hold_on_timeline` applies at once. Every replica compares the database's identity with the one recorded
  each minute, beside the marker, not only at its start, so a failover or a database swapped in under running servers
  is held (or, `false`, has its new timeline recorded) within a minute, as the setting says then. Skipped while the
  database is fenced or a planned move's mark is there (the move's own steps decide). The doctor says the value and
  where it comes from; the guide says what to choose for each kind of service.
- **The doctor.** `database.role` (owner, or may create in the database and its public schema), `database.extensions`
  (made by the role or an administrator), `database.copy` (keys and triggers ready for copies, named when not),
  `database.replication_role` (a grant this build never needs: a warning while held), `database.role_limit` (the
  role's own limit against what the replicas running now, from `replica`, may open to PostgreSQL), `restore.marker`
  (this replica's last look, or that its outward work waits for the store) and `failover` (the setting's value and
  where it comes from). The guide's *Any PostgreSQL* has a row per service; every line about a service is marked as
  remembered, not run.

## D8. Git

The forge keeps its database in PostgreSQL, its eight kinds of objects in the object store, its sessions and issue
index in the database, and takes every key as a file written at start; its configuration file is generated at each
start in memory. Its one volume holds repositories and the task queue, and is backed up to the object store on a
schedule, with a restore drill. Its administrator is made without a person. The gate counts turns in the database
and runs as any number of replicas. A Code app, in its own repository, gives repositories, access by membership,
tokens, sign-in to the forge through Ferrum and CI runners; Cloud keeps running the forge.

## Not doing

pgcrypto, pgsodium or any encryption inside SQL; OpenBao as a required service; a shared filesystem for Git; Redis
or Valkey as a requirement; a database role per app.
