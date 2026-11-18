# What every app gives

The completeness contract for an app of Ferrum's: what a
person, an administrator and the platform can count on from every app, whoever wrote it. The parts a
program can see are checked on every pull request by `scripts/check_apps.py`, over the apps the lock
compiles in; the rest is reviewed, and walked in a browser (docs/platform/evidence).

How to follow it, step by step and with examples, is in the app developer's handbook: [docs/guides](guides/README.md).

## Checked on every pull request

| Part | What it is |
|---|---|
| A repository of its own | `ferrum-app-<name>`, under a licence Ferrum may ship (AGPL-3.0 or compatible) |
| A manifest | `app.yaml` with its id, name, version, publisher, summary, categories, licence, homepage and where to ask for support |
| A description | The manifest names its README, which the store and the install dialog show |
| A history | `CHANGELOG.md` says what each version changed, the version in the manifest included, and the README points at it (or lists that version in a changelog table of its own) |
| A scenario test | At least one `tests/*_it.rs` that runs the app's flows against a real PostgreSQL; the test kit ([testkit.md](testkit.md)) is how a suite starts |
| An interface, when it has one | `ui/dist` built, with `.source` saying which sources and which Ferrum-Frontend it was built from |
| A launcher icon the shell draws | Every `shell.app` contribution names an icon `server/shell/runtime.js` knows |
| Operator settings, when it declares any | Native apps only, each with a key, a variable, a known type and a doc; the app requires the platform that reads them; its code reads the variables with `ferrum_kernel::env::var` (*Operator settings*, below) |

## Reviewed

- **A home that says what needs attention**, and offers the next step when there is nothing yet:
  every home page is walked empty as well as full.
- **Record pages for what people work with**, through the platform's record page, so lists, forms,
  actions, history, comments and permissions behave the same in every app. An action says what it
  did.
- **Roles for the jobs people do**, each allowed what that job needs; sensitive fields in a field
  group (bank details, identity numbers), masked rather than hidden where a person needs to know
  there is one; a person's own records reachable through owner fields.
- **Pages for people outside the workspace** where they deal with it: customers, suppliers, partners,
  citizens, lessees, guests, each signed in to the portal and seeing only what their address
  belongs to.
- **Reports for the figures its users are asked for**, with the parameters they would choose.
- **Upgrades that keep what was made before them working**: a release that adds a standard account,
  a field or a default reaches records made before it (or says how to), and nothing that was
  installed is redone.
- **Phone width and accessibility basics**: a form's labels name their fields, the pages fit 375 px,
  and the colours hold in dark mode.
- **A walk on a running server** before a release that changes flows people use, with what it found
  and fixed recorded.

## The vault

An app keeps its keys and secrets in the platform's vault, through a handle of its own, and never holds a key of the
platform's (docs/security.md, *The vault*).

**Its handle.** An app registers under its id (`HookRegistry::app(id, packages, |h| ...)`), and takes its handle inside
that registration (`h.vault()`): a `Vault` that opens only what is this app's, and works with the kernel the registry
boots and no other. Keep it in what you register. Asked for anywhere else, it is a handle nothing accepts, and the
server does not start.

**Its verbs**, each for a workspace (`Scope::Workspace`) or the installation (`Scope::Installation`):

| Verb | What it does |
| --- | --- |
| `set`, `rotate`, `remove`, `describe`, `list` | its secrets, under a rule: whether its code may read the value, the hosts it may be sent to, what a sandbox has of it, how long the version before still verifies |
| `reveal` | a secret's value, where its rule allows it |
| `sign`, `verify` | HMAC (a value) or Ed25519 (a key pair the vault made); `verify` takes the version before during its overlap |
| `derive`, `derive_at` | a key for one purpose from the app's own key, and again at an older version for what was made with it |
| `seal`, `open` | data sealed under the app's own key for a context, kept wherever the app keeps it; it names the version that sealed it, and opens after a rotation |
| `carry`, `carry_at` | a secret put on an outbound request, for the hosts its rule names; `carry_at` with a version the app names, and says which it carried |
| `adopt` | sets a secret only where it has no current version: what a replica found in its environment, copied once however many copy it at once; recorded with who copied it |
| `origin` | how the current version of one of the app's installation-scope secrets was written, as the installation's record says |
| `field` | the value of a secret field its own packages declare (a field one app adds to another's entity is the adder's), read in the caller's transaction and recorded; another app's is refused |

**A secret it manages itself.** `manage(k, scope, name, true)` marks a secret of the app's own as the app's to manage
(a published signing key, a token it rotates on its own schedule); `false` gives it back to people. While marked, the
administrator's and operator's routes refuse to set, rotate, remove it or change its rule, and an operator's regenerate
too, each with `409` naming the app; the app's own handle keeps using and changing it, and every description says
`managed_by`. The mark travels with the workspace's vault (a move, a sandbox) and goes when the secret does.

`sign_at` signs with a version the app names (none: the current one) and says which it used, and `public_key` gives a
kept version's public half, so an app that publishes its keys (a token issuer's) names each signature's key. `key_version`
says which version of its own key `seal` seals with now, and `Vault::sealed_version` which one sealed what it kept: an
app seals again what an older version sealed, so that version can go.

**Its own transaction.** Each verb runs in a transaction of its own, on a side pool of connections (an eighth of the
replica's pool, at least four), never in the caller's: a verb's result and its record stand whether or not the caller's
transaction then commits. So a `set` made before a business transaction that rolls back leaves the secret set: set it
after the commit, or remove it when the work fails. A `reveal`, `sign` or `carry` is recorded even when the work it
served fails. A verb does not see what the caller's open transaction wrote and has not committed. Under load a verb waits
for a side connection, never for the caller's own, so it cannot hold up the caller's transaction by itself; do not call one
while holding a lock on what that verb takes (the secret's own lock). The one verb in the caller's transaction is
`field`, which reads the app's secret field with the record it belongs to; with the vault closed it is refused, as the
kernel's own readers are. A workspace's key and an owner's key are never made inside a caller's
transaction: a secret field written there first has them made on the vault's own connection, committed, so a vault call
on another connection before the caller commits never waits on it (an app's `ready()` that made its keys first on a
connection of its own is not needed).

## Using another app's operations

**Offering one.** An app offers an operation to other apps as an action on one of its records, or on a record it
extends, with a contract: `hidden`, so that it is no button, and in the catalog all the same (docs/logic/capabilities.md).
An action a package adds to another package's entity is the adding package's: `{adder}.{entity}.{action}`, at the
adder's version. Its `permission` is for whoever reaches it directly: a person through the API, an agent, a process.

**Declaring a use.** The package that needs another app's operation says so, within what it depends on:

```yaml
package: bookings_payments
depends: [bookings, commerce]
uses:
  - {capability: commerce.sales_invoice.request_payment, version: "^0.6"}
```

The catalog refuses a use of a package it does not depend on, and a version that is not a range. The install screen
lists the uses with the app's other permissions, and an upgrade that adds one asks again; disabling the app takes its
packages, and with them its uses, away.

**Calling it**, in the caller's transaction, with the vault handle the app's registration took:

```rust
let ask = Invocation::new("commerce.sales_invoice.request_payment", json!({"id": invoice, "params": {"gateway": gateway}}));
let receipt = k.call_tx(&handle, ctx, tx, &ask.key(format!("deposit:{booking}"))).await?;
```

- **Granted, or not.** A package of the app installed in the workspace declares the capability, or the call is
  `forbidden`. The operation must be installed, at a version within a range declared, or it is
  `capability_unavailable`. An action's params are those it declares, each it needs given, or the call is `invalid`.
- **For the person.** It runs as the system on behalf of the person in `ctx`, within their zone: the use, approved at
  install, stands in for the operation's `permission`, and nothing else is reached through it. The person is the
  receipt's actor, and the maker of what it makes.
- **Once, and the app's own.** The key is the app's (`{app}:{key}`): two apps never meet on one, and a key one caller
  took answers no other. The same key again answers what was done, as the first call's result, whoever the app acts
  for now (a job with no person, another person): the app did it, and may need the id of what it made. With other
  input it is refused (`input_changed`).
- **Done or refused, never left waiting.** An operation that would wait for an approval is refused. A refused call
  leaves nothing of itself in the caller's transaction: the call runs inside a savepoint, and on any refusal (the
  operation's own check failing after it wrote, an approval it would wait for) everything it wrote, its receipt and
  any approval it parked are rolled back to it, and its key is free again. What the caller wrote before the call stays,
  so a caller may carry on without it ("no deposit: confirm the booking anyway") and commit. Calls nest: a call made
  inside another app's operation is undone alone, and a refusal of the outer one takes the inner ones with it. A
  caller that does not carry on rolls back as usual, and everything goes together. The cost is two statements per call
  (`savepoint`, then `release`), three on a refusal.
- **Recorded.** The receipt names the app (`logic_effect.caller`) beside the person, and finding an operation
  (`GET /api/admin/operations/{id}`) shows it as each effect's `caller`.
- **Seen.** `GET /api/admin/apps/connections` (administrators, docs/api.md) lists, for the workspace, what each
  installed package uses with its range and the version installed, the hooks apps run on other packages' records, and
  each app's calls of the last 30 days by capability, with a count and the latest time.

The handle is what proves which app calls: keep it inside the app's crate (`pub(crate)`), never handed to another.

## Files an app keeps for its own work

What an app keeps for its own work in the platform's file store (built artifacts, databases' backups, a customer's
managed objects) is the workspace's, counted and deleted as any file, but not the workspace's content to hand out: it
is a kind of file the app declares in its registration, `<app id>.<name>`, and keeps with that kind's handle.

```rust
// Inside HookRegistry::app("cloud", …): the kind is the app's, and only its handle reaches files of it.
let backups = h.file_kind("backup"); // cloud.backup
// Later, with the kernel and the workspace: tied to an id of the app's choosing (the project, the build).
let kept = backups.keep(k, tenant, project, "db.dump.0", "application/octet-stream", &bytes).await?; // {id, file_name, size, sha256}
let (name, content_type, bytes) = backups.read(k, tenant, file).await?; // or info(), to answer with it as a stream
backups.release(k, &mut tx, tenant, &files).await?; // in the caller's transaction: the bytes go after the deletion lag
```

- **Nobody else reaches them.** The platform's routes for files refuse a file of a kind to everyone, administrators
  included, as a file that is not there: no download, copy, deletion, print or tie to a record by its id, and no
  listing names it. Another app's handle finds nothing of it. A kind whose app left the build stays so.
- **Their only way out is the app's.** An app answers with one from its own site (`SiteResponse::file`, streamed by the
  server: from a site of the app that declared the kind, and no other), to the people its own rules let have it. The
  workspace's export leaves them out (`files_left_out` in its manifest, per kind), so the app owes its customer a way
  to have them: a download of a backup, an artifact, an object, from its own pages.
- **Counted and deleted as any file**: the workspace's storage quota, the deletion lag, a move, a sandbox, the
  workspace's deletion, the doctor and the restore drill treat them as they treat every file.
- A kind is declared once, by one app, inside its registration; anything else stops the server.

## A companion's sign-in

An app that runs a program beside Ferrum (its *companion*: the Cloud app's forge) has people sign in to it through
the workspace with OpenID Connect (docs/api.md, *Signing in to programs beside Ferrum*), in two steps. Here a companion
is a program people sign in to (`ferrum_kernel::oidc::Companion`, `HookRegistry::openid_companion`); a program whose
files the server renders is a service (*Declaring a service*: `companion::Companion`, `HookRegistry::add_companion`),
and one program can be both.

**As it registers**, it says what the companion needs, under a name of its own (lowercase letters, digits and dashes):

```rust
hooks.app("cloud", &packages, |h| {
    let vault = h.vault();
    h.openid_companion("forge", Arc::new(Forge));
    // …keep `vault` for the declaration below
});

#[async_trait]
impl ferrum_kernel::oidc::Companion for Forge {
    /// Who is let in: a member with this right (some records are enough). Everyone else is told access_denied.
    fn right(&self) -> (&str, Action) { ("Code Repository", Action::Read) }
    /// What places the person at the companion: put in the ID token and the user-info beside the platform's claims.
    async fn claims(&self, k: &Kernel, tenant: Uuid, user: Uuid) -> Result<Map<String, Value>> {
        let login = forge_login(tenant, user);
        Ok(json!({"preferred_username": login, "email": format!("{login}@people.ferrum.invalid")}).as_object().cloned().unwrap_or_default())
    }
}
```

A claim the platform sets (`iss`, `sub`, `aud`, `azp`, `exp`, `iat`, `auth_time`, `nonce`, `amr`, …) is refused, and
the sign-in with it. Registered outside an app's registration, or twice, the server does not start.

**For each workspace**, its code declares the companion's client with its own handle, whenever it starts the companion
or its address changes, and removes it when the companion goes: `vault.companion(&k, tenant, "forge", &[redirect_uri])`
→ `Client {client_id, secret, redirect_uris, …}`. The return addresses (one to ten) are `https://…`, or `http://` on
this machine's loopback, and are matched exactly. The client's secret is the app's own secret `oidc-client.<name>` in
that workspace, made by the vault once, managed by the app (people's routes refuse to change it), revealed to the app's
code to hand to the companion: `vault.reveal(&k, Scope::Workspace(tenant), &client.secret)`. Declaring again changes
nothing that is the same; it is safe from every replica at once. `vault.remove_companion(&k, tenant, "forge")` takes the
client and its secret away: every sign-in through it ends, and the app is told (below).

**When a sign-in ends** (the person signed out of that browser, everywhere, was deactivated, deprovisioned or erased,
the workspace suspended, the client taken away), the app hears `ferrum.companion.signed_out` through an event sink:
`{user, subject, client_id, app, companion, session, reason}`. Ferrum stops answering for it at once; the companion's
own session is the app's to end.

**The forge, as Cloud configures it** (Cloud's `src/code/sign_in.rs` and `docs/operating-code.md`; the protocol checked
with the real Forgejo 17.0.1, scripts/oidc_rehearsal.sh): an OpenID Connect authentication source per workspace
(`ferrum-<16 hex of the workspace>`, discovering `https://<the workspace's main address>/.well-known/openid-configuration`),
which Cloud's controller writes every minute in the forge's own `login_source` table with the client it declared (Forgejo
has no call for it); the forge takes a new source at its first sign-in and a changed one at its next start. Before
Ferrum gives the forge a person's token (in `claims`), Cloud makes their account, or checks the one it made, bound to
that source and their subject (`source_id`, `login_name`), and keeps its id: the forge finds them by the subject and
makes no account and links none by itself (`[oauth2_client] ENABLE_AUTO_REGISTRATION = false`, `ACCOUNT_LINKING =
disabled`, `[service] DISABLE_REGISTRATION = true`). Access is granted, and an account locked, only as the account whose
id Cloud holds; an account under the person's name that Cloud did not make for them is given nothing, and their sign-in
is turned back. The email claim is still an address only that account has. On `ferrum.companion.signed_out`, Forgejo 17 has no call that ends one person's sessions (a new password
leaves them, and a session in use never runs out): the one thing that ends them at once is locking the account
(`prohibit_login`, as Cloud does on a departure), which refuses every session of it until it is lifted, and lifting it
lets the same sessions back in. So lock it on every reason; for `access_ended` keep it locked; for the others lift it
in `claims`, which Ferrum calls only while issuing that person a token for a live session of theirs, before the forge
signs them in.

## A domain the workspace owns

An app that acts on a domain a workspace names (mailboxes in its mail domain, mail sent as it) asks
`Kernel::domain_proven(tenant, domain)` in its settings hook and again in its upkeep, and refuses or stops while it is
false: it is true only while the workspace holds the domain and its owner's DNS record proved it, never for two
workspaces. The refusal tells the administrator to claim the domain under Administration → Addresses with the target
`mail` and add the DNS record shown there (docs/operations.md, *Custom domains*, Domains for mail).

## Upkeep

An app's periodic work is `pub async fn maintenance(k: &Kernel, tenant: Uuid) -> Result<()>`: its upkeep of one
workspace. The platform (`server/src/packages.rs`) keeps a row of `upkeep` for each active workspace that has at least
one of the app's packages installed, and runs the app for it about once a minute, on whichever replica has a slot free.
What it promises the app, and what it asks of it:

- **One workspace a call.** It never lists workspaces. It is called wherever any one of its packages is installed: an
  app that needs to know which asks `k.registry.schema(tenant)`, as before. A workspace suspended or moving is not
  called; one uninstalled is called no more.
- **Safe to run again.** It runs again after a replica stops or is lost, or ends a run after its lease (five minutes)
  when another replica has run it since, and runs for different workspaces on several replicas at once. Each step must
  be idempotent: claim rows with `for update skip locked`, compare and set, or take a transaction lock per workspace;
  twice must be the same as once.
- **Within 270 seconds.** Past it the call is dropped, its open transaction rolls back, and the run counts as failed;
  each run in a row that hits the limit doubles the wait before the next, up to an hour. Keep budgets per step (Comms'
  `RETRY_BUDGET`, `POLL_BUDGET`), and queue a job for work that takes longer or calls outside for long.
- **Its error is recorded.** Return the first failure after logging it and running the other steps: it is kept on the
  workspace's row, which its administrators read at `GET /api/admin/upkeep`, and counted by `/metrics`
  (`ferrum_upkeep_failing`) and the doctor. A returned error is due again after the usual minute.
- **As that workspace's work.** A call outside is the workspace's (a sandbox's sends nothing).
- **Work of the whole installation has no hook of its own** (a partition of a table every workspace writes, a purge
  across workspaces): do it from the call for whichever workspace comes first, at most once a day on a replica (a
  `static` day) and under `pg_try_advisory_xact_lock`, so that replicas do not do it together. ERP's journal
  partitions and Operations' telemetry partitions are done so.

## Operator settings

What an operator chooses for the whole installation and an app's code in the server reads (an address of a service, a
limit) is a setting of the server's registry, declared in the app's manifest. It is kept in the server's `setting`
table as `<app id>.<key>`, changed with `ferrum task set`, a settings document or the Operations screen, and listed in
the operations guide under the app (docs/operations.md, *Configuration*). It is not the settings page (`settings:`), which
each workspace's administrators fill.

```yaml
platform: ">=0.3.9, <0.4"        # the platform that reads operator settings
operator_settings:
  - key: office_url              # kept as drive.office_url
    env: FERRUM_OFFICE_URL       # the variable that sets it: the environment's value wins over the table's
    type: text                   # text (the default), bool, int, json, or choice (with values)
    default: ""                  # as the variable would say it; "" for the app's own default
    applies: start               # start (the default), or live: every replica takes a change within seconds
    sends: FERRUM_OFFICE_SECRET  # only for an address given an environment's secret (below)
    doc: The editor server as the server reaches it.
```

- **Read it with `ferrum_kernel::env::var(env)`**, never `std::env::var`: it gives the environment's value when the
  environment sets the variable, else the table's, else nothing (the app's own default), and a setting marked `live` as
  it is now. `std::env::var` sees the environment alone, so the table's value never reaches it; `scripts/check_apps.py`
  fails an app that declares a variable and reads it that way.
- **Never a secret.** A manifest has no field for one, and a variable the platform's registry holds as a secret is
  refused. An app keeps its secrets in its own vault (*The vault*, above), each bound by its rule to the hosts it is
  sent to (`Rule.hosts`, put on requests with `carry`), so that moving an address never moves the secret with it.
- **An address given an environment's secret** says so (`sends:` the secret's variable): while that secret is in the
  environment, the table's value of the address is not used, so that a row in the database never sends the
  environment's secret elsewhere. A workspace's own credential sent to an address (a connection's, a secret field's)
  is bound to its host in the vault in the same way.
- **One owner each.** The server adds every compiled-in app's declarations to its registry before it reads any setting,
  and does not start when two owners declare one key or variable, naming both. An entry of the platform's registry
  that moves to an app (`app:` in `server/src/settings.yaml`) is replaced by that app's declaration of the same
  variable, without a clash: until the app declares it, it is read from the environment only. Only a native app has
  operator settings.

## Declaring a service

A program an app runs beside the server (a meeting server, a mail server, an engine of its own) that shares a secret or
a setting with the server takes them from files written as it starts, from the database and the vault alone
(docs/operations.md, *Files for the programs beside the server*). The app declares it inside its registration: its name,
the files it writes, the secrets the vault makes for it, and a function that is given the app's own vault handle and the
settings and returns the files.

```rust
// Inside HookRegistry::app("comms", …): the service is the app's, and so are its secrets.
h.add_companion("meet", Arc::new(Meet));

struct Meet;

#[async_trait]
impl Companion for Meet {
    fn files(&self) -> &'static [&'static str] { &["livekit.yaml"] }
    fn made(&self) -> &'static [&'static str] { &["meet.secret"] }
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        r.made("meet.secret", Rule { reveal: true, overlap_secs: 86_400, ..Default::default() }).await?;
        let mut yaml = Zeroizing::new(String::from("port: 7880\nkeys:\n"));
        for v in r.versions("meet.secret").await? {
            yaml.push_str(&format!("  \"ferrum.{}\": \"{}\"\n", v.version, std::str::from_utf8(&v.value).unwrap_or_default()));
        }
        Ok(vec![File { name: "livekit.yaml".into(), bytes: Zeroizing::new(yaml.as_bytes().to_vec()), secret: true }])
    }
}
```

- **Names.** A service's name is lowercase letters, digits and dashes, unique on the installation: one declared by two
  owners stops the server, naming both. Its files are plain names (letters, digits, `.`, `_` and `-`, not beginning
  with a dot), and a render returns exactly those, each once. `ferrum task services` lists every service, with its owner,
  files and the secrets the vault makes for it.
- **Settings**: `r.setting(name)` (the server's value, by key or variable, `None` where nothing sets it) and
  `r.needs(name)` (refused, naming the setting, where nothing sets it). Declare them in `operator_settings`.
- **Secrets** of the app's own, in the installation's scope. `r.made(name, rule)` makes one the service declares in
  `made()`, where there is none: 32 random bytes as 43 characters of base64url, marked as the app's to manage (a secret
  of that name a person set is refused). `r.secret(name)` gives the current value, `r.versions(name)` the current one and
  the one before while its overlap lasts. Only where the secret's rule says `reveal: true`; each given is on the record
  as `render`, naming the service, never the value. A file that holds one says `secret: true`: one that says otherwise
  and holds a value the render was given is refused.
- **Where it belongs.** `refusal(r)` says, in plain words, why this installation may not have the service rendered
  (`None`: it may). It is asked before every render and every rotation, which are then refused with nothing written
  and nothing made, and `ferrum task services` shows it. A service that writes back a key its program made and handed
  over keeps it with `r.keep(name, rule)` (marked as the app's once, its value as it is; nothing made) and refuses where
  the vault's record (`vault().origin(…)`) does not say the program handed it over.
- **Rotation.** The operator rotates what the vault made (`ferrum task vault_rotate level:service service:<name>`); the
  app's code goes on using its handle. A program that can hold two values is given both (`versions`), and the app
  presents the version before while the overlap lasts (the current `version - 1`, while `describe` says
  `previous_until` is still to come), with `sign_at` or `carry_at`: a program not restarted yet still takes it.
- **Its deployment**: in Kubernetes an init container of the server's image runs `ferrum task render service:<name>
  to:/run/companion` into a memory-backed volume the program mounts read-only; in Compose a one-shot service does, into a
  volume the two share. The chart's helpers `ferrum.renderInit` and `ferrum.renderVolumes` give a bundled program both,
  and `ferrum.declaredServices` lists the services the server's image declares.

## How the check works

`scripts/check_apps.py` reads `cargo metadata` for the packages that come from CBFerrum's app
repositories, and for each that has an `app.yaml` beside its `Cargo.toml` (an app, or an add-on of
one) checks the parts above in that checkout. It prints each app and what it lacks, and fails if any
lacks something.
