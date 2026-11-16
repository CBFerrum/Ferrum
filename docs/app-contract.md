# What every app gives

The completeness contract for an app of Ferrum's (specification §7.2, FERRUM-UX-004): what a
person, an administrator and the platform can count on from every app, whoever wrote it. The parts a
program can see are checked on every pull request by `scripts/check_apps.py`, over the apps the lock
compiles in; the rest is reviewed, and walked in a browser (docs/platform/evidence).

## Checked on every pull request

| Part | What it is |
|---|---|
| A repository of its own | `ferrum-app-<name>`, under a licence Ferrum may ship (AGPL-3.0 or compatible) |
| A manifest | `app.yaml` with its id, name, version, publisher, summary, categories, licence, homepage and where to ask for support |
| A description | The manifest names its README, which the store and the install dialog show |
| A history | `CHANGELOG.md` says what each version changed, the version in the manifest included, and the README points at it (or lists that version in a changelog table of its own) |
| A scenario test | At least one test under `tests/` that runs the app's flows against a real PostgreSQL |
| An interface, when it has one | `ui/dist` built, with `.source` saying which sources and which Ferrum-Frontend it was built from |
| A launcher icon the shell draws | Every `shell.app` contribution names an icon `server/shell/runtime.js` knows |

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
platform's (docs/security.md, *The vault*; docs/plan/stateless.md, D4).

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
| `carry` | a secret put on an outbound request, for the hosts its rule names |
| `field` | the value of a secret field its own packages declare (a field one app adds to another's entity is the adder's), read in the caller's transaction and recorded; another app's is refused |

`Kernel::secret` (any app's secret field) still works and is noted, and goes in the next release.

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
`field`, which reads the app's secret field with the record it belongs to; with the vault closed it reads the old store
with the old key, as the kernel's own readers do. A workspace's key and an owner's key are never made inside a caller's
transaction: a secret field written there first has them made on the vault's own connection, committed, so a vault call
on another connection before the caller commits never waits on it (an app's `ready()` that made its keys first on a
connection of its own is not needed).

**What it made with the old key (R1 only).** An app that sealed or keyed things of its own with the old key
(`settings.secret_key`, deprecated) keeps them working through its handle until they lapse, with no key in its hands and
nothing wider. Each names a context of the app's own: its id, then one of ` -.:/|`, then more (`cloud-dns-key`,
`helpdesk mail token`, `pos snapshot`); not one that another app's longer id also begins (`cloud-ops-…` is `cloud-ops`'s,
not `cloud`'s), not one that begins with a workspace's id, and never for an app whose id is a word the platform's own
labels begin with (`message`, `connection`, `ferrum`, …). Anything else is refused. They work from the copies the vault
adopted, so they keep working once the key is gone from the environment, each use is noted for `ferrum task
legacy_key`, and they go in R2 with the key:

| Call | What it does |
| --- | --- |
| `open_old(k, context, sealed)` | what the old key sealed for `context`, opened with every old key |
| `seal_old(k, context, plain)` | `plain` sealed with the old key for the column the app's previous release reads, while the key is in the environment; `None` once it is gone |
| `old_fingerprints(k, context, data)` | `data`'s fingerprint under each old key, as `SecretKey::fingerprint` made it: to check what the app issued, never to issue anything new |

What each compiled-in app does in its repository: Commerce asks `Kernel::seals_secret_values()` in place of whether the
old key is set (a message's secret values are sealed with the vault now); Helpdesk checks the mail tokens 0.11 issued with
`old_fingerprints` (`helpdesk mail token`, `helpdesk mail token hash`); POS signs its snapshots with a key it derives
(`derive`), the server never checks their old signatures, and a till comparing one sees it change once; Cloud opens and
writes what it sealed with `open_old` and `seal_old` (its contexts begin `cloud-`). Until each has, the old key's field
stays public: apps that still read it are given the same key once it is gone from the environment.

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
    env: FERRUM_OFFICE_URL       # the variable the app read until now: it still works, and wins, through R1
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
