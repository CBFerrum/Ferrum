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

## How the check works

`scripts/check_apps.py` reads `cargo metadata` for the packages that come from CBFerrum's app
repositories, and for each that has an `app.yaml` beside its `Cargo.toml` (an app, or an add-on of
one) checks the parts above in that checkout. It prints each app and what it lacks, and fails if any
lacks something.
