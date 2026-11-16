# Secrets and the vault

Every password, token and key an app holds lives in the platform's vault, under the app's own name, and is used
through a handle only the app's registration is given. The contract is [docs/app-contract.md](../app-contract.md),
*The vault*; the design is [docs/plan/stateless.md](../plan/stateless.md), D3 to D5; the operator's side is
[docs/operations.md](../operations.md), *The vault*; what it protects against, and what not, is
[docs/security.md](../security.md), *The vault*. This guide is how an app uses it. The code is
`crates/kernel/src/vault/` (`mod.rs` for the handle, `seal`, `open` and `derive`; `secrets.rs` for the secrets' verbs;
`kept.rs` for secret fields; `keyed.rs` for the old key's calls).

## What the vault is, in one paragraph

Keys form a hierarchy: a slot's key opens the installation key, which wraps one key per workspace (and one for the
installation's own scope), which wraps one key per *owner* in that scope (`platform`, an app's id, or `plugin:<name>`),
which seals each secret. Every key below a slot is random, versioned and stored only wrapped, in schema `vault` under
row-level security that only the vault module's own transactions satisfy. Values are sealed with AES-256-GCM in the
server, bound to their place (scope, owner, name, version); nothing is encrypted by SQL. An app sees none of this: it
holds a handle, and the handle's verbs act on its own secrets only.

## Your handle

An app registers under its id, and takes its handle inside that registration:

```rust
// The server registers each compiled app this way (server/src/packages.rs); so must your tests.
hooks.app("comms", &packages, |h| ferrum_app_comms::register(h));
hooks.own("comms", &packages);          // the app's packages: their secret fields are its own

// Inside register (ferrum-app-comms/src/lib.rs):
pub fn register(hooks: &mut HookRegistry) {
    let site: Arc<dyn ferrum_kernel::Site> = Arc::new(site::CommsSite);
    keys::remember(&site, hooks.vault());   // Comms' own handle, kept beside the site it registers
    // ...
}
```

- `HookRegistry::app(id, packages, f)`: `id` is lowercase letters, digits and dashes, 1 to 63 characters, beginning with a
  letter or digit, and not `platform` (`app_id_ok`, `crates/kernel/src/vault/mod.rs:150`). Once per app, never inside
  another app's registration.
- `HookRegistry::vault()` inside `f` gives a `Vault` that opens only this app's own and works only with the kernel booted
  from that registry. Asked for outside an app's registration it returns a handle nothing accepts, and the server does
  not start: "a vault handle was asked for outside an app's registration" (`crates/kernel/src/hooks.rs:363`).
- A handle used with another kernel is refused at its first use: "this vault handle was not given out by this server".
- There is no other way to get one. No public function takes an owner and returns that owner's secrets; the module's
  doc tests prove that copying, asking the kernel or the registry for another app's, or turning a handle into another
  owner's does not compile (`crates/kernel/src/vault/mod.rs:17-54`).

**Keeping it.** Keep the handle with what you register. One process may hold kernels of several registries (a test's
replicas, the two set-ups of `ferrum start`), each accepting only its own registry's handle. Comms keeps each
registration's handle keyed by the address of the site it registered, and finds the right one from `k.hooks.site("comms")`
(`ferrum-app-comms/src/keys.rs`, `remember` and `handle`). Metrics keeps the last one for the process and says that is a
simplification (`ferrum-app-metrics/src/keys.rs`). Either way, say in the error what is wrong when it is missing:
"Comms was registered without its vault handle: register it as the server does, hooks.app(\"comms\", …)".

**Scopes.** Every verb takes a `Scope`: `Scope::Workspace(tenant)` for what belongs to one workspace (an account's
password, a room's guest key), `Scope::Installation` for what belongs to the whole installation (the key LiveKit and the
server share, an engine's token). A workspace's secrets go with it when it moves, are copied to a sandbox by their rule,
and are deleted with it. The installation's are the operator's to see.

## Secrets: names, rules, values

**A name** is lowercase letters, digits and `_ . -`, 1 to 100, beginning with a letter or digit. Names the kernel's own
stores keep, and `legacy.key.…` (the old keys it adopted), are refused (`check_name`, `secrets.rs:183`). Use a short
dotted name that says what it is for: `meet.secret`, `engine.token`, `forge.database_url`.

**A rule** (`vault::Rule`) is kept with each version:

| Field | What it says | Limits |
|---|---|---|
| `reveal` | Whether the app's code may have the raw value (`reveal`). `false`: it is only used (signs, verifies, is carried on a request) | Default `false` |
| `hosts` | The hosts the value may be sent to by `carry`: `api.example.com`, or `*.example.com` for its subdomains (which does not include `example.com` itself) | At most 20, lower-case host names |
| `sandbox` | What a sandbox of the workspace has of it: `Blank` (default: nothing), `Regenerate` (a value of its own, of the same kind, shown to nobody), `Keep` (this value, sealed with the sandbox's keys) | |
| `overlap_secs` | After a rotation, how long the version before still verifies, signs (`sign_at`), carries (`carry_at`) and is given to a render | 0 to 30 days (`MAX_OVERLAP_SECS`) |
| `expires_at` | When it stops being usable | A time to come |

**A value** (`vault::New`): `New::Value(Zeroizing<Vec<u8>>)` (at most 16 KiB, `crate::secret::MAX_SECRET`),
`New::Random(n)` (16 to 64 random bytes, kept as base64url text), or `New::Ed25519` (a key pair the vault makes; its
private half never leaves unless the rule reveals it). A `Value` signs as an HMAC-SHA256 key; an `Ed25519` pair signs
with Ed25519 (`vault::Kind`).

**What anyone may read of a secret** is `vault::Described`: `owner`, `workspace`, `name`, `kind`, `version`, `rule`,
`set_at`, `set_by`, `last_used` (a day), `public_key` (an Ed25519 pair's), `previous_until` (until when the version
before still verifies) and `managed_by`. Never the value: its type has no field for it.

## The verbs

Each takes `&Kernel` and a `Scope` (every signature is in `crates/kernel/src/vault/`):

| Verb | What it does |
|---|---|
| `set(k, scope, name, value, rule)` → `Described` | A new secret, or a new version with a new rule |
| `rotate(k, scope, name, value)` → `Described` | A new version under its rule; the version before verifies for the overlap. Not found when there is none |
| `adopt(k, scope, name, value, rule, by)` → `bool` | Sets it only where there is no current version, under the secret's lock, so replicas doing it at once write it once; recorded as `adopt` by `by` (`"environment"`) |
| `remove(k, scope, name)` → `bool` | Every version |
| `describe(k, scope, name)` → `Option<Described>`; `list(k, scope)` → `Vec<Described>` | Never a value |
| `reveal(k, scope, name)` → `Zeroizing<Vec<u8>>` | The value, only where the rule says `reveal: true`; else `Forbidden`: "…'s rule keeps its value in the vault: it is used, never revealed". Recorded every time |
| `sign(k, scope, name, message)`; `sign_at(…, version)` → `(version, signature)` | HMAC-SHA256 or Ed25519 with the current version, or a kept one |
| `verify(k, scope, name, message, signature)` → `bool` | The current version, or the one before while the overlap lasts |
| `public_key(k, scope, name, version)` → `Option<String>` | An Ed25519 version's public half, base64url |
| `carry(k, scope, name, &url, &how)`; `carry_at(…, version)` | The header that puts the secret on a request to `url`'s host, only when the rule's `hosts` names it |
| `manage(k, scope, name, managed)` | Marks it the app's to manage (below) |
| `derive(k, scope, purpose)`; `derive_at(…, version)` → `Derived` | A key for one purpose from the app's owner key (HKDF-SHA256); nothing stored |
| `seal(k, scope, context, data)` → `String`; `open(k, scope, context, &sealed)` | Data sealed under the app's owner key, to keep in the app's own tables |
| `key_version(k, scope)`; `Vault::sealed_version(&sealed)` | Which owner-key version seals now, and which sealed a value |
| `field(k, tx, tenant, entity, record, field)` | The value of a secret field the app's own packages declare, in the caller's transaction |
| `companion(k, tenant, name, &uris)`; `remove_companion(k, tenant, name)` | A sign-in client for the app's companion: [10](10-companions-and-sign-in.md) |
| `origin(k, name)` | How an installation secret's current version was written (`adopt`, `make`, `set`, `rotate`, and by what): for a service that must know who made a key |

### Each verb runs in a transaction of its own

Every verb but `field` runs on a side pool of connections (an eighth of the replica's pool, at least four), in its own
transaction, never in the caller's ([docs/app-contract.md](../app-contract.md), *Its own transaction*). So:

- A `set` made before a business transaction that rolls back leaves the secret set. Set it after the commit, or remove
  it when the work fails.
- A `reveal`, `sign` or `carry` is recorded even when the work it served fails.
- A verb does not see what the caller's open transaction wrote and has not committed.
- Do not call a verb while holding a lock that verb takes (the secret's own advisory lock).

`field` is the exception: it reads with the record, in the caller's transaction.

### When the vault is closed

Every verb refuses with `Error::Unavailable` (or `Retryable`), saying why: the vault cannot open its root
(`ferrum task vault_status` and the doctor say why). Turn that into a sentence a person can act on, and put the cause in
the log, as Comms does:

```rust
// ferrum-app-comms/src/keys.rs
fn unavailable(e: Error) -> Error {
    match e {
        Error::Unavailable(why) | Error::Retryable(why) => {
            tracing::debug!("Comms' keys: {why}");
            Error::Unavailable("Comms cannot make or open guest links right now: its keys are unavailable".into())
        }
        e => e,
    }
}
```

Never fall back to something weaker (a plain value, a key of your own) when the vault refuses.

## Reveal, hosts and carry

Choose `reveal: true` only where a library or a program must hold the raw value: a password a mail library sends, the
key in a configuration file. Otherwise keep it `false` and let the vault use it:

- **To sign or check**: `sign`, `sign_at`, `verify`. Comms signs LiveKit's tokens with `meet.secret` without ever
  holding it (HS256 is HMAC-SHA256 over `header.payload`; `src/meet.rs`, `signed`).
- **To send it**: `carry` builds the one header that carries it, for one request. `vault::Carry` is `Bearer`
  (`authorization: Bearer <secret>`), `Basic(user)` (`authorization: Basic <base64 of user:secret>`) or
  `Header { name, prefix }` (`<name>: <prefix><secret>`). It is refused, `Forbidden` "this credential may not be sent to
  <host>", unless the rule's `hosts` names the URL's host, and refused when the value is not one line of text.

Metrics carries its engine's token so (`ferrum-app-metrics/src/engine.rs`, `ask`; shortened, its error mapping and the
environment's token left out):

```rust
let (client, url) = k.vetted_client(&address, true, Duration::from_secs(ENGINE_SECS), "ferrum-metrics").await?;
let version = crate::keys::sending(k).await.map_err(|e| crate::keys::refused(e, &host))?;
let (_, (header, token)) = crate::keys::vault()?
    .carry_at(k, Scope::Installation, crate::keys::TOKEN, &url, &Carry::Bearer, version).await?;
let r = client.post(url).header(header, token.as_str()).json(body).send().await?;
```

**Binding to hosts is what makes an address safe to change.** An address is a setting anyone with the operator's
token can change; a secret bound to the host it was given for will not follow it there. When the address moves, the
app refuses and says which secret's hosts to change, as Metrics does: "Metrics' engine token may not be sent to <host>:
the engine's address moved, so change the hosts of Metrics' secret engine.token to match". The operator changes them
with the rule route (`PUT /api/operator/secrets/{owner}/{name}/rule`, [docs/api.md](../api.md)).

A secret a person gives (an API key for a workspace's account) is bound the same way: give its rule the hosts of the
service it is for, and never `reveal` it unless a library cannot take a header. A workspace's credential for an outside
service is better kept in a connection, whose secrets the platform carries itself: [11](11-integrations.md).

## Rotation and overlap

`rotate` makes a new version under the secret's rule; from then on `sign` and `carry` use it, while `verify` still
accepts the version before until `previous_until`. For a program that holds both versions at once (LiveKit's keys, the
Metrics engine's token file), present the version before while the overlap lasts, so that a program not restarted yet
still takes it:

```rust
// ferrum-app-metrics/src/keys.rs: the version to send.
pub(crate) async fn sending(k: &Kernel) -> Result<Option<i32>> {
    let d = handle()?.describe(k, Scope::Installation, TOKEN).await?;
    Ok(d.filter(|d| d.managed_by.is_some() && d.previous_until.is_some_and(|t| t > chrono::Utc::now())).map(|d| d.version - 1))
}
```

Rotation of the keys above a secret (owner, workspace, installation) is the operator's and the vault's; the app's code
does not notice it. What an app *sealed* with an older owner key is the app's to seal again (below).

## A secret the app manages itself

Some secrets are the app's to change, not people's: a signing key it publishes, a token it rotates on its own schedule,
a key a service's render made. `manage(k, scope, name, true)` marks one; `false` gives it back. While marked, the
administrator's and operator's routes refuse to set, rotate, remove it or change its rule, with `409`, error
`managed_by_app` and the app's id in `app` (`vault::MANAGED_BY_APP`); the app's handle keeps using and changing it, and
`describe` says `managed_by`. The mark travels with a move or a sandbox and goes when the secret does. A companion's
client secret and a service's made secrets are marked for you ([10](10-companions-and-sign-in.md)).

## Secret fields

A field of type secret in a package's entity is kept in the vault as its declaring app's (a field one app adds to
another's entity is the adder's). The app reads it with its own handle, in the caller's transaction:

```rust
// ferrum-app-comms/src/keys.rs
pub(crate) async fn field(k: &Kernel, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<Zeroizing<String>>> {
    handle(k)?.field(k, tx, tenant, entity, record, field).await
}
```

Another app's field is refused, set or not: "E.f is not X's: an app reads only the secret fields its own packages
declare" (`crates/kernel/src/vault/kept.rs:807`). Each read is recorded as a reveal. The server says which app owns which
package (`HookRegistry::own`); register yours with it in tests too, or `field` refuses. `Kernel::secret` still reads any
app's field and is deprecated: it goes in the next release, and `scripts/secret_paths.py` keeps the list of apps that call
it, which is empty today (docs/registry/secret-paths.md).

## Keys for a purpose, and sealing

**`derive(k, scope, purpose)`** gives a key for one purpose (lowercase letters, digits and `_ . : -`, up to 100) from the
app's owner key: the same purpose gives the same key until the owner key rotates, and nothing is stored. Its
`Derived::version()` says which owner key made it; `derive_at` makes it again from an older version while that version is
kept. Use it for an HMAC over what the app issues (POS signs its snapshots with a derived key since 0.7.5). Name the
version beside what you made, so you can check it after a rotation.

**`seal(k, scope, context, data)`** seals up to 1 MiB (`vault::MAX_SEALED`) under the app's owner key for a context (up
to 200 characters, no control character: say what it is kept for, `comms_meeting.guest_key/<room id>`). The result is
text, `fv1.<version>.<base64url>`, to keep in the app's own table; `open` with the same scope and context returns it,
and refuses anything sealed for another owner, scope or context, or changed: `Invalid` "this does not open here: it was
sealed for another place, or changed". Comms seals each room's guest key so (`src/keys.rs`, `seal_guest_key`,
`guest_key`).

**Seal again what an older version sealed.** An owner-key version that sealed or derived something kept outside the
vault is kept when a newer one comes (it is marked `outside`), so that what it made still opens. To let it go, compare
`Vault::sealed_version(&kept)` with `key_version(k, scope)` in upkeep and seal again, a batch to a transaction, taking
rows with `for update skip locked` so that replicas share the work: Cloud's `reseal` in `src/keys.rs` is the pattern.

**A sandbox and a move.** A sandbox is a new workspace with keys of its own: what the app sealed for the live workspace
does not open there (the scope is part of what a value is bound to), so `open` answers `Invalid`. Treat that as "not
here", never as an error that stops a page: Comms answers a guest link that does not open as no link. A move keeps the
workspace's id and carries its vault with a transfer, so sealed values open at the target.

## Adopting what the environment held (R1)

Through R1 a variable that set a secret still wins, so that a rollback behaves. An app that read a secret from the
environment copies it into its vault once, bound to the hosts it serves now, and uses the environment's while it is set:

```rust
// ferrum-app-metrics/src/keys.rs
let rule = Rule { hosts: vec![host], ..Default::default() };
handle()?.adopt(k, Scope::Installation, TOKEN, New::Value(Zeroizing::new(token.as_bytes().to_vec())), rule, "environment").await?;
```

`adopt` writes only where there is no current version, under the secret's lock, so replicas starting together, or a
service's render making the secret at that moment, write it once. Comms does the same for LiveKit's key and secret and
the mail administrator's password (`src/keys.rs`, `Operator`). In R2 the variables stop being read and the adoption
code goes.

## What the old key made (R1 only)

`FERRUM_SECRET_KEY` is the kernel's alone; no app is given it (`KernelSettings::old_key` gives nothing back). What an app
sealed or fingerprinted with it in an earlier release it reaches only through its handle, for contexts of its own, until
R2 ([docs/app-contract.md](../app-contract.md), *What it made with the old key*):

| Call | For |
|---|---|
| `open_old(k, context, sealed)` | What the old key sealed for `context`, opened with every old key, the adopted copies too |
| `seal_old(k, context, plain)` | Sealing for the column the app's previous release reads, while the key is in the environment; `None` once it is gone |
| `old_fingerprints(k, context, data)` | `data`'s fingerprint under each old key, to check what the app issued; never to issue anything new |

A context is the app's id, then one of ` -.:/|`, then more (`cloud-dns-key`, `helpdesk mail token`); not one another app's
longer id also begins, not one beginning with a workspace's id, and never for an app whose id is one of the kernel's
words (`ferrum`, `message`, `connection`, `platform`, `plugin`, … in `KERNEL_WORDS`, `keyed.rs`). Each use is noted for
`ferrum task legacy_key`. A new app never needs these.

## On the record

Every `set`, `rotate`, `remove`, `manage`, `reveal`, `sign`, `carry` (as `send`), `adopt` and render is recorded with the
owner, the name, the verb and the version, never a value: a workspace's on its own audit trail, the installation's in
`vault.event` (`ferrum task vault_events`). Uses are recorded one by one up to 100 an hour per secret and verb
(`USES_PER_HOUR`), then counted and summarised when the hour is over. `last_used` is kept to the day.

## What an app must never do

| Never | Why | What catches it |
|---|---|---|
| Put a secret in an operator setting, a variable it adds, or `app.yaml` | A setting is readable and changeable by whoever holds the operator's token, and kept plain | The manifest has no field for one; `scripts/check_apps.py` refuses unknown setting fields; `server/src/settings.rs` refuses an app declaring a platform secret's variable |
| Read the vault's tables, or set `ferrum.vault` | Bypasses the owner check | Row-level security shows no row; `scripts/secret_paths.py` fails any app that names `vault.*` or `ferrum.vault` outside tests |
| Use a cryptography crate of its own (`hmac`, `ring`, `aes-gcm`, `ed25519-dalek`, `jsonwebtoken`, `rsa`, …) | Keys it makes are outside the vault | `scripts/secret_paths.py` (the `CRYPTO` list; hashes alone are allowed); existing uses are in a baseline that may only shrink, docs/registry/secret-paths.md |
| Write `unsafe` | Inside one process the vault keeps owners apart by the shape of the code | `scripts/secret_paths.py` |
| Log, format or return a value | A log is read by many; an error reaches a person | Reviewed, not checked. The vault's own types hide values from `Debug` (`New`, `Derived`, `Client`); keep values in `Zeroizing` and never `{:?}` a header |
| Print a value in a test's output | CI logs are kept | Reviewed, not checked. Metrics' `external_it` checks that no table holds its token in plain text |
| Fall back to a weaker mode when the vault refuses | Fail closed | Reviewed, not checked |
| Call the people's or operator's functions (`vault_set`, `operator_set`, …) or open the vault | They name an owner; only the platform's code may | `scripts/secret_paths.py` (`WAYS_IN`) |

## Tests

An app's suite boots a kernel the way the server does: the vault open, the app registered by its id owning its
packages. From Metrics' `tests/external_it.rs`:

```rust
let db = Db::connect(DbConfig { url: url.clone(), max_connections: 16, ..Default::default() }).await?;
sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await?;
// The vault open, as on a server whose database address has a password (the credential slot).
let opener = ferrum_kernel::vault::Opener {
    database_url: url.clone(),
    file: "/nonexistent/root.key".into(),
    kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },   // cheap Argon2id for tests
    ..Default::default()
};
ferrum_kernel::vault::open(&db, opener, false).await?;
let mut hooks = HookRegistry::default();
ferrum_domain::register(&mut hooks);
let mut vault = None;
hooks.app("metrics", &[], |h| {
    ferrum_app_metrics::register(h);
    vault = Some(h.vault());          // the test keeps a handle too, to look at what the app keeps
});
let k = Kernel::boot(db, catalog, hooks, settings, None).await?;
```

- Drop the `vault` schema with `public` between runs: it is a schema of its own.
- The database address must carry a password for the credential slot.
- Register with `hooks.own(id, &packages)` too when the app reads secret fields (Campaigns' `tests/campaigns_it.rs`).
- A suite that gives the kernel the old key (`KernelSettings { secret_key: Some(…), .. }`) keeps working: the kernel
  takes it in and empties the field.
- Test the refusals, not only the happy path: the value not revealed, the host refused, a closed vault's message.

[15](15-testing-and-ci.md) covers suites in full.

## Checked by

- `scripts/secret_paths.py` (CI, and `scripts/local_ci.sh audit`), with its tests `scripts/test_secret_paths.py`; its
  baseline docs/registry/secret-paths.md.
- `scripts/check_apps.py`: no secret among operator settings.
- The vault's suites in the platform: `crates/kernel/tests/secret_it.rs` and its mutants in `scripts/mutants.py`; the
  compile-fail doc tests in `crates/kernel/src/vault/mod.rs`.
- Logging, error text and test output holding no value: reviewed, not checked.

## See also

- [07 Stateless apps](07-stateless-apps.md), [10 Companions and sign-in](10-companions-and-sign-in.md),
  [11 Integrations](11-integrations.md), [13 Security rules](13-security-rules.md)
- [docs/app-contract.md](../app-contract.md), *The vault*; [docs/security.md](../security.md); [docs/api.md](../api.md),
  *The vault and the installation's secrets* and *Secrets*; [docs/operations.md](../operations.md), *The vault*
