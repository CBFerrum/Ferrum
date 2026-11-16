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

### D5, first half: the environment's secrets (built)

- **Which.** The registry marks each with `vault:` and its kind: `session` (`JWT_SECRET`), `compared` (bootstrap,
  metrics and operator tokens) or `library` (mail password, object store keys, model provider key, audit sink key).
  Each belongs to `platform` at the installation's scope, under its setting's key. Not in this half: `FERRUM_SECRET_KEY`
  and what it seals, the KMS options, the database addresses, and secrets an app crate reads itself (Meet's, the mail
  administrator's, Metrics' engine token, the Portugal signing key): declared, not moved.
- **Precedence in R1**: the configuration (the environment, or the file in development and tests) over the vault; the
  vault closed, everything as before. A replica starting to serve stores what its environment sets and the vault lacks,
  once (`Vault::adopt`: set only where no current version is, under the secret's lock), recorded as `adopt`.
- **The server's handle**: `HookRegistry::platform_vault()`, once, before any app registers (else the server does not
  start). It reads the library secrets before the kernel boots (`Vault::reveal_at_start`, with the registry that gave it
  out), which the mail sender and the audit sink need; acts for the operator on the installation's scope
  (`operator_set`/`_rotate`/`_remove`, `installation_secrets`); and makes plugins' handles (`Vault::plugin`).
- **Sessions: a secret key with an id, not a key pair.** Only this server verifies these tokens, so a pair adds a key
  format and nothing else; HS512 as before. The vault's keys are secrets `session.<n>`; a token names its key (`kid`).
  A rotation makes the next and gives each key before it an end (`expires_at`): the longest token lifetime then, plus
  five minutes. Replicas read the keys every 30 seconds and at once (at most once a second) on a key they do not hold.
  `JWT_SECRET`, where set, still signs without a `kid` (an older build reads those); its copy verifies them after.
- **Compared tokens** are kept as SHA-256 digests and checked with `Vault::verify` (an HMAC under the digest), so they
  are never revealed and a dump's form is not the token; a rotation's overlap lets callers change over.
- **Library secrets** are revealed to the platform alone, at start. One sent to an address (`sends:`) is bound by its
  rule's `hosts` to the host it was set for; moving that address takes it again or clears it (cleared, address
  written, set: an interruption leaves none). They apply at the next start, listed as settings that apply at start are.
- **The outbound verb**: `vault::carried(hosts, host, how, secret)` builds the header for one request; `Vault::carry`
  uses it for a vault secret (recorded as `send`), the plugin host for its declared credentials and for `vault:<name>`,
  and connections for theirs (their reach still checked at every hop). A plugin's handle never reveals.
- **A workspace's vault**: removed in the transaction that deletes the workspace (`delete_tenant`, `delete_sandbox`,
  `finish_move`, a move's dropped copy). A sandbox from this installation gets keys of its own and each secret by its
  rule; one from another installation starts empty. A move carries the vault with a **transfer**: at the source,
  `vault_transfer` wraps each version of the workspace key with a key made for it (`vault.transfer`), shown once and
  never stored; the operator gives it in `FERRUM_MOVE_SOURCE_SECRET_KEYS`. The copy's transaction opens the workspace
  keys with it, wraps them with the target's installation key and copies owner keys and secrets as they are (bound to
  the workspace id a move keeps); the point of no return compares a digest of the source's vault.
- **A fenced database** (read-only transactions) still answers the vault's uses; it records none of them.

### D5, second half: the kernel's stores (built)

- **Who owns what.** A secret field is the app's whose package declares it; a field one package adds to another's entity
  is the adder's. The server says which app owns each package it compiles in (`HookRegistry::own`, outside any app's
  registration; a package claimed by two stops the server). A package installed at run time, with no code of its own,
  is the app's it came with (`app_release`): no code holds that app's handle, so its values are set and cleared by people
  and read by nobody. The platform's own packages, and those an operator adds from a directory, are the platform's.
  Everything else in these stores is the platform's: connections' credentials and tokens (used on a caller's behalf,
  never read back), message channels', single sign-on's, SAML's, second factors, webhooks', approvals' secret
  parameters, the push key.
- **Where.** Each value is a secret of its owner in its workspace's scope (the push key in the installation's), named
  `<store>.<40 hex of SHA-256 of its place without the workspace>`, so that a move and a sandbox keep the name. Its rule:
  a field's value is revealed to its app's code; no other is revealed. A sandbox: webhooks `regenerate`, second factors
  `keep`, the rest `blank`. A connection's: the hosts its description reached when its secrets were given.
- **Writes** go to the vault in the transaction that writes the store's row. While `FERRUM_SECRET_KEY` is configured the
  old column is sealed with it as well, as the previous build reads it; with none it holds `vault` (`sealed:vault` for
  a webhook), which opens with no key. With the vault closed, as before it (sealed with the old key, refused without
  one). Never plain where the vault is open: a webhook's secret was, without a key.
- **Reads.** `vault.kept` holds, for each version, a SHA-256 of the old column as it was written beside it. The vault's
  value is read when the old column still holds that (or the mark); otherwise the old column, with the old keys: a value
  not moved yet, or one the previous build changed since. Each such read is noted (`legacy_key_use`, `open:<store>`).
- **The move**: `Kernel::vault_move`, in the hourly upkeep and `ferrum task vault_move`. Each waiting value is taken in a
  transaction of its own, under its secret's lock and its row's, both only tried (a value a person or another replica
  holds is left for the next round, so nothing is moved twice), and read back from the vault before it commits. A plain
  webhook secret is sealed (or marked) in its row as it moves. What the vault keeps for a row that is gone (deleted by
  either build, an approval decided) is removed. `vault_waiting` counts what waits, by store.
- **The previous build** reads the old columns with `FERRUM_SECRET_KEY` and `FERRUM_RETIRED_SECRET_KEYS`: kept current
  for as long as a key is configured, since every write seals there too and `reseal_secrets` still rotates them. With no
  old key it finds marks and refuses each, as it refuses a value sealed with a key it lacks (in production it does not
  start without the key at all). Nothing is deleted from the old stores; a plain webhook secret is replaced by its seal.
- **Readers.** `Vault::field` reads a secret field of the handle's own owner, and refuses any other, set or not.
  `Kernel::secret` and `KernelSettings::secret_key` keep working in this release, deprecated: the function carries the
  attribute and notes each caller's file at run time; the field is documented so (the attribute would warn in every
  test that configures a key). `scripts/secret_paths.py` lists every compiled app that names either, and the doctor
  names the callers seen. Both go private in R2.
- **Use, not reveal.** These values are opened only by kernel code that puts them on a request, signs or checks with
  them. A connection's secrets are had only while its description reaches the hosts they were given for, so changing
  its hosts asks for them again; each request still goes through the connection's guard and `vault::carried`.
- **Rotation.** Owner, workspace and installation keys rotate these secrets as any other. `reseal_secrets` and the
  retired keys keep rotating what the old columns hold (each then reads from there until the move takes it again).
- **The old key's end.** Built in the old key's last uses, below: `Kernel::legacy_key` (`ferrum task legacy_key`, the
  doctor) says it can go once every key is adopted, nothing waits to move and older builds are refused.
- **Moves, sandboxes, deletion, dumps.** A workspace's values travel with its vault rows (the first half); old columns
  holding the mark pass a move as they are, and the source's old keys still open the rest. A sandbox has each by its
  rule; a deleted workspace's go with its vault; a deleted record's, channel's, connection's, provider's, token's or
  person's second factor go in the transaction that deletes it. A dump holds sealed rows and wrapped keys only.
- **Not in this half**: plugins' settings, and what the old key keyed or sealed for a while: the old key's last uses.

### D5, the old key's last uses (built)

A new installation needs no `FERRUM_SECRET_KEY`, and one that has it takes it out of its environment
(crates/kernel/src/vault/keyed.rs; docs/operations.md, "Retiring the old key").

- **Keys of the platform's own.** What the old key keyed or sealed for a while has a key derived from the platform's
  owner key for one purpose (`derive`): a workspace's for opt-out links, erased opt-outs (`erased:v<version>:…`),
  sign-ins' states and verifiers, public addresses of files, connectors' account fingerprints, receipts' digests and
  messages waiting with secret values (`seal`, under the platform's key, for those); the installation's for processes'
  payloads (the codec is not told which run a payload is for), each naming the version that sealed it. Every version
  that derived or sealed one is kept (`outside`): links, addresses and erased opt-outs are checked under each, so they
  outlive a rotation; receipts' digests and connectors' fingerprints are made with the current version only, so a
  request repeated across a rotation is answered as a new one. With the vault closed each is refused; nothing falls back
  to the old key once it is retired.
- **The window.** The previous build checks these with the old key alone, so while the key is configured and an older
  build may still start, they are made with it, as before; a rollback finds every link, sign-in, address and receipt
  still good. Older builds are refused by a floor (`old-key-retired`): `ferrum task legacy_key retire:yes` puts it in
  place, once every key is adopted and nothing waits to move; so does the first thing a replica makes without the key.
- **Adopted once.** Each key the environment configures (`FERRUM_SECRET_KEY`, `FERRUM_RETIRED_SECRET_KEYS`, and
  development's key derived from `JWT_SECRET` before the vault) is a secret of the platform's in the installation's scope
  (`legacy.key.<id>`, used, never revealed, its name refused to the verbs), written at the first start with the vault open
  under the secret's lock, so replicas starting together write it once. Whatever the old key sealed or keyed is opened
  and checked with these copies once the environment no longer has it.
- **Apps.** `settings.secret_key` stays public through R1 (apps compiled in at their pinned revisions read it): it holds
  the environment's key, else the one it last had (adopted), else a key of the platform's own. An app moves onto its
  own handle; what it sealed or keyed with the old key itself it keeps through the R1-only calls `open_old`, `seal_old`
  and `old_fingerprints`, for contexts of its own (docs/app-contract.md, *The vault*).
- **Plugins' settings** are sealed under each plugin's own key in the workspace's vault, beside the old key while it is
  configured; the move takes those from before (sealed with the old key, or plain). Never stored plain.
- **Production** no longer requires the key where the vault opens; development with the vault open no longer derives
  one. Compose, Kubernetes and the chart pass it through where it is set.

## D6. Object storage only

The local directory is refused outside development and tests. Docker Compose bundles an S3-compatible store. The
store's address is a setting; its keys are a platform secret, or none where the pod has an identity. The doctor
checks versioning, lifecycle and encryption at rest. Objects are backed up beside the database, and a drill restores
both to one moment. Downloads stay through the server; short-lived direct links are a setting, never for entities
marked for recorded reads.

### D6, decided while building it (16 November 2026)

- **One seam.** Loco's storage is now `Objects` too (`server/src/objects.rs`, `Files`): what handlers, the shell, the
  App Manager and the kernel's file store keep goes through the same reads, writes and deletions as streams, uploads
  and presigned links. The store is built once, from the settings, with the same keys for both.
- **A directory, existing or new** (`server/src/store.rs`, `decide`). Outside development and tests a server without a
  bucket starts only when the newest object its database names (its newest `file_object`, else the newest interface's
  first file) is in that directory: a new database names none, a wrong or empty directory lacks it, and a database that
  has kept its files in a store (`file_store`, written by any start with a bucket) is refused. No setting a mistake can change makes that true. `require_object_storage`
  still refuses outright.
- **Moving** (`file_store` 'moving', then 'store'; `file_move`; `ferrum task files_move`). Writes to the store from a
  start with a bucket on; reads store first, directory second (`Objects::find`, Loco's reads, streams, ranges,
  journals' listings, parts of an upload begun before); deletions from the directory first, then the store, so a copy
  made meanwhile sees the file gone and deletes its own. The move: one at a time (a session advisory lock on the direct
  pool); per file streamed, read back, length and SHA-256 compared, recorded; never written over a different object,
  never deleting from the directory; uploads, probes and the restore marker left out. Confirmed only with nothing
  failed, no live `replica` that started before files began to move or is of an older build, and one more pass after
  that check finding nothing new (at most eight passes). The one writer it cannot see, a build from before `replica`,
  leaves files in the directory that `files_move` copies when run again.
- **Keys or identity.** Unchanged for AWS (no endpoint). With an endpoint and no keys the start is refused, as before,
  unless the machine names an identity (`AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_CONTAINER_CREDENTIALS_FULL_URI` or
  `_RELATIVE_URI`); then the S3 library's chain finds them. The doctor and `s3_set_up_bucket` sign their bucket
  requests with the same chain (`reqsign`, the crates OpenDAL uses, at its versions).
- **One moment.** Names are never used again for other bytes (a workspace's file by a new id; an interface's or an
  app's by its digest), so a database restored to T needs only that T's objects exist. Deletions wait
  `files_delete_after_days` (`object_release`, swept every ten minutes, each row held while its object is deleted, never
  while a restore or a move holds outward work). A key that is written again (an interface's file) is "not there" while
  its deletion waits, so whoever asks writes it again, which takes the deletion back; a write waits for a sweep holding
  its row. The backup *relies on versioning* (a lifecycle rule keeps old versions as long as the backups reach): a
  version is only ever made by a deletion, so it costs nothing until one, where a copy job would double every file,
  need keys and a schedule, and lag behind; losing the bucket is the provider's replication's. Never roll the bucket
  back: the restore marker would go back with it. The drill (`deploy/drill.sh`): a dump restored (or a database already
  restored), then `check_files digest:yes` (every `file_object` read through, length and SHA-256 against its row; app
  bundles looked for). No chart CronJob: nothing to copy.
- **The doctor**: `storage.directory` (error-level), `storage.moving`, `storage.deletions` (`files_delete_after_days`
  against `backup_reach_days`), `storage.versioning`, `storage.lifecycle`, `storage.encryption`, `storage.public` (a
  request without keys, and the policy's status) and `storage.clock`.
- **Compose's store: SeaweedFS** (docs/references/object-stores.md: Garage keeps no versions and does not encrypt).
  `deploy/store/start.sh` makes the keys and the encryption key at its first start and hands the access key over in a
  volume the two share alone; the server keeps it in the vault and deletes the file; `s3_set_up_bucket` makes the
  bucket and its protections. The chart keeps taking an outside store: a store in the chart would not be highly
  available, and the chart's own database is.
- **Direct links**: off; `307` to a link signed for the store, at most 300 seconds; never for a recorded entity, an
  outside person, an active type, or while files move.
- **Not proved here**: IRSA's token taken by AWS STS and a role's policy on the bucket; GKE workload identity (GCS
  speaks S3 with HMAC keys only); a lifecycle rule taking effect (days); Compose itself (no Docker on this machine:
  `deploy/smoke.sh` was not run).

### D6, other programs' stores (for the apps slice)

Read on 16 November 2026 (Cloud `b6736e2`, Metrics engine origin/main `a68881e`, Comms `eb19eec`, Live `ce317bf`, Edge
`5fe1ea1`). Where the seam itself falls short for them: an app reaches files only through `Kernel::store_file`,
`read_file`, `file_info` and `outbox::release_objects`, whose keys must be `tenants/{tenant}/…`
(crates/kernel/src/files.rs:129, crates/kernel/src/outbox.rs:202); the raw store is `pub(crate)` (files.rs:95);
`store_file` takes a whole file in memory, `read_file` refuses one over 256 MiB (files.rs:49); and nothing adopts an
object another service wrote into the store as a file (recordings need it).

- **Cloud.** Its own OpenDAL client, S3 or a directory, for three stores (`src/controller.rs:47-76,132-135`;
  `deploy/k8s/controller.yaml:45-50,64-67`: `FERRUM_CLOUD_ARTIFACTS`, `_COPIES`, `_OBJECTS`, keys
  `FERRUM_CLOUD_S3_ACCESS_KEY_ID`/`_SECRET_ACCESS_KEY` from Secret `cloud-providers`): remove `store()`, the variables
  and the keys; its kernel already holds the platform's store (server/src/app.rs:494). Build artifacts
  (`src/build/controller.rs:367-374,454`, `sql/005_build.sql:59-72`) and resources' backups and exports
  (`src/resources/controller.rs:550-571,618`, `sql/002_resources.sql:114-130`): `store_file` with a system context, a
  file id in place of `location`, reads over 256 MiB streamed; `FERRUM_CLOUD_COPIES_REGION` (controller.rs:114-115)
  goes or stays an exception. Apps' object resources (`src/resources/objects.rs:87-113,360-372`,
  `sql/002_resources.sql:180-199`), with a delete sweep of their own that ignores `files_delete_after_days`: a
  `file_object` per version deleted through `release_objects`, or a kernel API for a store under a prefix; customers'
  own buckets (objects.rs:61-85) stay. The forge (`deploy/k8s/forge.yaml:2-3,62,68,89-93`): repositories and SQLite
  stay on the volume (D8); LFS, attachments and packages to prefix `forge/` by Forgejo's `[storage]` from `s3_*`. The
  TLS edge's ACME account and customers' certificates on a 1Gi volume (`deploy/k8s/edge.yaml:7-9,21,33-39,94`): Caddy
  with S3 storage under `edge-tls/` (private keys would then sit in the files bucket: decide first). Serving snapshots
  and build scratch (`deploy/k8s/serve.yaml:32,49,54-59`, `serve/src/node.rs:92-168`): rebuildable, stay.
- **The Metrics engine.** Reads CSV and Parquet from local paths, age by mtime, writes nothing (engine
  `src/lib.rs:95-105,296-297,567-579,667-673`, `src/main.rs:27-29`); here a bind mount and a ReadOnlyMany claim
  (deploy/metrics-engine.yml:23-24, deploy/k8s/metrics-engine/metrics-engine.yaml:60-67,92, the chart's
  templates/metrics-engine.yaml:55-62,87, values.yaml:377-386, docs/operations.md "Metrics' engine"): DataFusion's
  `object_store` with `aws`, `s3://` paths from `s3_*` with read-only keys or a pod identity, age from last-modified,
  egress to the store. It is the operator's input, not Ferrum's files: it may also stay outside.
- **Recordings.** Meetings: LiveKit Egress writes `ferrum/{tenant}/{room}/{rec}.mp4` to storage of its own and Ferrum
  keeps a `location` string (Comms `src/meet.rs:57-66,246-252,325-331`, `sql/005_meet_records.sql:15-28`; no Egress
  service in deploy/meet.yml): give Egress `file_outputs[].s3` from `s3_*` and the vault's keys (or its own identity)
  under `tenants/{tenant}/{file id}`, then adopt the object as a `file_object` (size and SHA-256 read as a stream) on
  the meeting's record: the missing kernel call above. Live records nothing yet (its replay link is a URL on an
  outside store, `src/broadcast.rs:125-128`, `src/upkeep.rs:88-106`); its relay's TLS and signing keys are files, not
  objects.
- **Edge certificates.** The gateway's key and journal (`gateway/src/config.rs:46-48,200-205`, `main.rs:41-81`) and
  the device ports' certificates, OPC UA PKI and firmware cache (`runtime.rs:168,193-199,517`,
  `adapters/transport.rs:195-204,512-525`, `adapters/opcua.rs:213,226`) stay on the site's box: it works offline and
  the key never leaves. Platform side, device firmware is a URL with its SHA-256 (`packages/edge/entities/edge.yaml:207-220`,
  `src/firmware.rs:32-37,167`): a file field stored with `store_file`, fetched by gateways through a new signed route
  answered with `read_file` (relay.rs:23-25 lists the routes passed). No CA keys anywhere; device certificates are
  fingerprints in the database (`src/site.rs:326-333`).

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
  in its query dropped. The old full addresses still win. A row in `setting` would otherwise choose where the server
  sends `DATABASE_URL`'s credential, which D2's rule for addresses given a secret keeps every other secret from (that
  rule cannot say it here: `DATABASE_URL` is no setting, and it would refuse every stored host). So a host from the
  table is used only on `DATABASE_URL`'s own host, any port, or when the address says `verify-full` with an
  `sslrootcert` (the mounted authority, or the address's own; the program's built-in roots certify anyone's names).
  Otherwise it is not used and never connected to: one error in the log at start, and a doctor warning
  (`database.direct_host`, `database.read_host`). The check is at the point of use (`server/src/app.rs`), so the
  registry's mechanics are unchanged; a host from the environment is used as before. An authority shared by a
  provider's customers still vouches for all of them. The two settings stay in the environment in `minimal`, as the
  migrate Job needs the direct host before the settings Job has run.
- **The certificate authority.** Applied as the configuration loads (Loco's `load_config` hook), before anything
  connects: `DATABASE_URL` as the configuration holds it gets `sslrootcert` and a mode that checks it (none or `prefer`
  becomes `verify-full`, `require` becomes `verify-ca` as libpq does; an address's own `sslrootcert` is left alone;
  `disable` or `allow` refuses to start), and the two full addresses get the same where they are used; the process's
  environment is never rewritten. One connection is tried first, so that a certificate that does not verify refuses
  the start in those words. A debug build looks for `db-ca.pem` in `FERRUM_TEST_RUN_DIR` when that is set, so that
  `server/tests/database_ca_it.rs` runs the whole start with the authority right, wrong and absent (the managed
  rehearsal); a release build looks only at `/run/ferrum`.
- **The dump.** `deploy/dump.sh`: pg_dump as the server's role with `ferrum.system` and `ferrum.vault` set for its
  session and `--enable-row-security`; the restore into an empty database in one transaction, without owners or
  grants, leaving out schemas the database has and comments on extensions. The chart's `import` mode passes the same
  setting as the `options` connection parameter and `--enable-row-security` as `pgDumpExtraOptions`. The vault's
  schema (D3) travels whole, its guards with it; where the copy's role has another password its credential slot does
  not open, and the operator runs `vault_rewrap` (old password or recovery key) or opens with a `file` or `kms` slot
  and makes the credential slot again (docs/operations.md, "The vault").
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
- **The rehearsal.** `scripts/managed_pg.sh` (a private PostgreSQL 17 with PgBouncer 1.21 or later in transaction mode
  beside it, an ordinary role owning only its databases, no grant, the extensions an administrator made in their own
  schema, TLS with its own certificate authority) and `scripts/local_ci.sh managed`, run only when named. Suites reach
  each database through the pooler, with each one's direct address by host (`FERRUM_TEST_DATABASE_DIRECT_HOST`, a
  test's own variable). It found that a source given as a pooler's address (a move's, a sandbox's, an extraction's,
  which start with session settings and a move holds a session lock on) failed in PgBouncer's words: such an address,
  and a `DATABASE_URL` that is a pooler without a direct address, is now refused saying to give PostgreSQL's own.

## D8. Git

The forge keeps its database in PostgreSQL, its eight kinds of objects in the object store, its sessions and issue
index in the database, and takes every key as a file written at start; its configuration file is generated at each
start in memory. Its one volume holds repositories and the task queue, and is backed up to the object store on a
schedule, with a restore drill. Its administrator is made without a person. The gate counts turns in the database
and runs as any number of replicas. A Code app, in its own repository, gives repositories, access by membership,
tokens, sign-in to the forge through Ferrum and CI runners; Cloud keeps running the forge.

### D8: signing in to the forge through Ferrum (built)

Ferrum is an OpenID Connect provider for the programs installed apps run beside it (crates/kernel/src/oidc.rs; docs/api.md,
*Signing in to programs beside Ferrum*). What was decided, and why:

- **The issuer is the workspace's address the request came to** (every address that serves one workspace's shell is
  one, each with the same subjects, and a code is good only where it was asked). Without a base domain: the
  installation's one workspace on its own host, or a domain of a workspace's own; a host several workspaces share is
  no workspace's, and answers nothing. A forwarding address serves nothing, and answers nothing. A sandbox answers
  nothing either, rather than being an issuer of its own: nothing a sandbox does reaches outside (`Kernel::outward`),
  and a companion is outside; its companions' clients are not copied, so none could be given a code there.
- **Keys unique within a workspace, never across the installation.** A client's id, a code, an access token and a
  nonce are looked up under the workspace the issuer names. The copier takes every table with a `tenant_id` unless a
  table is named to be left out, and refuses one with a key unique across workspaces: with keys like these, a copy
  that does not know these tables (an older build's) finishes, and this build takes the client it brought into a
  sandbox away, with its codes and sign-ins, telling nobody (they are the live workspace's). The build before
  (8a34644e) in fact makes no sandbox at all on a database this one migrated: it is a guest there, and refuses
  (scripts/window_rehearsal.sh shows it, and leaves nothing). Rejected: naming the workspace in another column, which
  every copy refuses as "named outside" its tables. A move carries the
  client, the sign-ins and the subjects, and the vault the signing key and the client's secret: the program goes on
  unchanged at the same address; the target tells the apps that the sign-ins it carried ended, their sessions having
  stayed behind.
- **An operator can turn it off**: the stored setting `openid`, at once; off, nothing of it answers and API clients'
  OAuth is as it was. The doctor says which, and how many clients there are.
- **A companion's client is its app's**, declared in code per workspace (`Vault::companion`), its secret the app's own
  managed secret; there is no consent page. The app names the right that lets a person in and the claims that place
  them. Dynamic registration stays as it was and never gives `openid`.
- **Ed25519.** The vault's own key kind (`oidc.signing`, a platform secret per workspace, versions with a day's
  overlap, `sign_at` naming each signature's version). The real Forgejo 17.0.1 accepted EdDSA, and ES256, and also a
  token with a signature of zeros: it never fetches `jwks_uri`, and checks the issuer, the audience, the expiry and the
  user-info's subject, trusting TLS to the token endpoint for the rest. Forgejo accepting anything, the vault's kind
  is the smallest choice; a relying party that verifies only RS256 would need an RSA key, which ring signs with but
  cannot make (OpenID Connect Discovery lists RS256 as required; Core exempts providers that answer only at the token
  endpoint, as this one does).
- **Nothing outlives the browser session**: no refresh tokens for companions; codes and access tokens are refused once
  the session or the person's access ended; the app is told within 15 seconds (`ferrum.companion.signed_out`) and ends
  the companion's own session. API clients' refresh tokens are as they were: they end with signing out everywhere, a
  new password or deactivation, not with one device's sign-out (their consent page says so).
- **The forge's side is Cloud's**: an authentication source per workspace, the forge's account name and an address
  only that account has as claims, and locking the account to end its sessions (docs/app-contract.md, *A companion's
  sign-in*). Rehearsed against the real Forgejo: scripts/oidc_rehearsal.sh.

## D1 and D2 closed: the environment's last readers (built)

What still decided anything from the environment after D5, and what a new installation still had to be given besides
the database's address (server/src/settings.yaml, docs/operations.md "What a replica still reads from its environment").

- **Apps declare their own settings**, in their manifest's `operator_settings` (key, env, type, values, default,
  applies, sends, doc): not `settings:`, which is already the settings page each workspace fills. Kept as
  `<app id>.<key>`, read by the app with `ferrum_kernel::env::var`. The server adds every compiled-in app's declarations
  before it reads a setting; one key or variable with two owners stops the start, naming both; an app that declares a
  variable the platform holds as a secret is refused (its secrets are its own vault's). Only native apps have them. The
  platform's 22 entries that belong to apps are under their app (`app:`, keys renamed `<app>.<key>`, never stored
  before), with `sends:` where an address is given an environment's secret (Meet's addresses `FERRUM_MEET_SECRET`, Hosted
  mail's `FERRUM_MAIL_ADMIN_SECRET`, Metrics' engine `FERRUM_METRICS_ENGINE_TOKEN`); the apps' secrets likewise. **They
  stay the environment's**: every app pinned today reads them with `std::env::var`, which a table's value never reaches
  (Drive's `archive.rs`, `office.rs`, `site.rs`; Comms' `meet.rs`, `hosted.rs`, `dns.rs`, `webmail.rs`; Metrics'
  `engine.rs`; the EU, GB, US, Malaysia, Romania and Portugal add-ons), so storing them would make the doctor say the
  table decides what nobody reads. An app's declaration replaces its entry when its release reads them through
  `ferrum_kernel::env::var` (scripts/check_apps.py fails one that declares and still reads them itself). The platform's
  file drops them in R2. The app API is 0.3.9 (the vault and operator settings).
- **A task's inputs** (`input:`): the move task's, neither stored nor counted by the doctor.
- **The key-management client.** `kms_transit`, `kms_kubernetes_role`, `kms_kubernetes_mount` and `kms_namespace` are
  stored settings (readable before the vault opens). Its token is never a setting: the file `/run/ferrum/kms.token`, or
  the pod's service account at its standard path; `FERRUM_KMS_TOKEN_FILE`, `BAO_TOKEN`, `VAULT_TOKEN` through R1. A
  file the server sends somewhere (a token, the service account's token) or trusts (a certificate authority:
  `/run/ferrum/kms-ca.pem`) is never named by the database. A stored address could otherwise send the mounted token to
  whoever wrote the row, so the vault's `kms` slot keeps the address it was made with (`params.api`) and the token goes
  nowhere else (a slot from before names none and is asked as before); keys from the environment given as ciphertext
  are unwrapped, and `kms_wrap` wraps, only at an address the environment gives. `FERRUM_SECRET_KEY_COMMAND` stays in
  the environment: a command line is never read from the database.
- **Telemetry**: the four `OTEL_*` settings are stored and the headers a `library` secret bound to the collector's host
  (`sends:` on both addresses); applied in code as Loco sets the log up, which it does after the first set-up's
  `after_context` has read the settings and opened the vault, so nothing is sent before (D1). The SDK still reads its
  other variables itself.
- **A fresh installation**: `JWT_SECRET` and every other secret of the server's Secret are optional (chart, deploy/k8s,
  Compose with only the database's two passwords). The chart makes `<fullname>-vault-key` once (`lookup` keeps its
  value, `helm.sh/resource-policy: keep` keeps it past an uninstall; a named Secret instead), and mounts it with the
  database's and the key-management service's files in one projected volume at `/run/ferrum`, every source optional.
  A server's start (`vault::settle`, not a task's) adds the `file` slot where the key is mounted and the vault has none,
  and makes the `credential` slot again where it no longer opens and another slot opened the vault, each once under the
  root's lock; never one that was removed, never replacing a file slot. `vault_rewrap` and `vault_add_slot
  kind:credential with:` take `file:<path>`. A task runs while object storage is not set yet (on a fresh database the
  settings that set it are what `apply_settings` writes); the server does not. The direct and read hosts may be
  `DATABASE_URL`'s own parameters (`ferrum_direct_host`, `ferrum_read_host`), taken out as the configuration loads and
  trusted as the address is; `environment: minimal` renders them so. A release build reads no test's variable.
- **The proof** (`scripts/one_input.sh`, `scripts/local_ci.sh oneinput`): the real binary, `env -i DATABASE_URL=…` with
  the environment named on its command line, a library in front of libc's `getenv` listing every name asked for; the
  run fails on any name found set but `DATABASE_URL`, or read and declared neither by the registry (read through R1) nor
  by the baseline of what stays (docs/registry/environment-only.md, written by scripts/settings.py, which only
  shrinks).

## Not doing

pgcrypto, pgsodium or any encryption inside SQL; OpenBao as a required service; a shared filesystem for Git; Redis
or Valkey as a requirement; a database role per app.
