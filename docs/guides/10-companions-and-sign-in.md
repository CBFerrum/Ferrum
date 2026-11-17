# Companions and sign-in

Some apps run a program beside the server: Comms runs LiveKit for meetings and a mail server (Stalwart) for hosted
mailboxes, Metrics runs its engine for external sources, Cloud runs a forge (Forgejo). Such a program needs two things
the platform gives:

1. **Its files**, written as it starts, from the database and the vault alone: a *service* (`ferrum_kernel::companion`,
   `HookRegistry::add_companion`). A secret the server and the program share is made once, by the vault, and typed by
   nobody.
2. **People signed in to it** through the workspace, with OpenID Connect: a *companion* in the sign-in sense
   (`ferrum_kernel::oidc`, `HookRegistry::openid_companion`).

One program can be both. The contract is [docs/app-contract.md](../app-contract.md), *Declaring a service* and *A
companion's sign-in*; the operator's side is [docs/operations.md](../operations.md), *Files for the programs beside the
server* and *Programs beside Ferrum signing in through it*; the protocol is [docs/api.md](../api.md), *Signing in to
programs beside Ferrum*.

Before you run a program of your own, look for a mature open-source one to run instead (LiveKit, Stalwart, Forgejo are
all run, not written): [13](13-security-rules.md), *Dependencies and licences*.

## Part 1: a service's files

### Declaring a service

Inside the app's registration (`HookRegistry::add_companion`, `crates/kernel/src/hooks.rs`):

```rust
// ferrum-app-comms/src/lib.rs, inside hooks.app("comms", …)
hooks.add_companion("meet", Arc::new(meet::Service));
hooks.add_companion("mail", Arc::new(hosted::Service));
```

A service implements `companion::Companion` (`crates/kernel/src/companion.rs`):

| Method | Required | What it says |
|---|---|---|
| `files(&self) -> &'static [&'static str]` | yes | The files it writes, by name: exactly what a render returns, each once |
| `made(&self) -> &'static [&'static str]` | no | The secrets of the app the vault keeps for it, at the installation's scope |
| `refusal(&self, r) -> Result<Option<String>>` | no | Why this installation may not have it rendered, in plain words; asked before every render and rotation |
| `group_reads(&self) -> bool` | no | `true`: files are 0640 for a program running as another user of the group; otherwise 0600 |
| `render(&self, r: &Rendering) -> Result<Vec<File>>` | yes | The files' bytes |

Names: a service's is lowercase letters, digits and dashes, 1 to 63 characters, unique on the installation; a file's is
letters, digits, `.`, `_` and `-`, 1 to 100, not beginning with a dot; each `made` name must be a name the vault takes.
Anything else, or a name two owners declare, stops the server at start, naming both (`declaration_problems`,
`HookRegistry::settle`). `files()` and `made()` are static so that `ferrum task services` lists them without touching a
secret.

### Rendering

`render` is given a `Rendering` (`companion.rs`), made by the kernel for one render:

| Call | What it gives |
|---|---|
| `r.vault()` | The app's own handle (the same verbs as [08](08-secrets-and-the-vault.md)) |
| `r.kernel()` | The kernel |
| `r.service()` | The service's name |
| `r.setting(name)` | A setting's value by key or variable, as the server resolved it (the environment's, else the table's), trimmed; `None` where nothing sets it to something. Only the server's registry: a service never reads past it into the environment |
| `r.needs(name)` | The same, or refused: "the service X needs the setting N, which is not set: ferrum task set key:N value:<value>" |
| `r.made(name, rule)` | Makes a secret the service declares in `made()`, where there is none: 32 random bytes as 43 characters of base64url, under `rule`, marked as the app's to manage. Renders and replicas at once make one. One a person set there is refused |
| `r.keep(name, rule)` | Takes a secret the app holds already (adopted from the environment, or set by a person) as the service's from now on: marked, given `rule`, its value as it is. Nothing made; refused where there is none |
| `r.secret(name)` | The current value, only where the rule says `reveal: true`; recorded as `render` |
| `r.versions(name)` | The current value and the one before while the overlap lasts, newest first: for a program that holds two values at once |

A `File { name, bytes: Zeroizing<Vec<u8>>, secret }` (or `File::new(name, bytes, secret)`) says whether it holds a
secret. **A render refuses before anything is written**: the vault closed, a setting it needs that nobody set, a secret its
rule keeps from its owner, a file it did not declare (or one it declared missing), or a file that says `secret: false`
and holds a value the render was given (values of 16 bytes or more are looked for).

Settings a render reads are the app's operator settings: declare them in `operator_settings` ([07](07-stateless-apps.md)).

### A worked example: Metrics' engine token

The whole service, from `ferrum-app-metrics/src/keys.rs`:

```rust
pub const TOKEN: &str = "engine.token";
const OVERLAP_SECS: i64 = 86_400;   // a day to restart the engine's pods in

#[async_trait::async_trait]
impl Companion for Service {
    fn files(&self) -> &'static [&'static str] { &["token"] }
    fn made(&self) -> &'static [&'static str] { &[TOKEN] }

    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        let rule = Rule { reveal: true, overlap_secs: OVERLAP_SECS, ..Default::default() };
        match r.vault().describe(r.kernel(), Scope::Installation, TOKEN).await? {
            // None yet: the vault makes it, bound to the engine's host.
            None => {
                let url = r.needs("metrics.engine_url")?;
                let host = host(&url).ok_or_else(|| Error::invalid("metrics.engine_url is the engine's address, such as http://metrics-engine:5180"))?;
                r.made(TOKEN, Rule { hosts: vec![host], ..rule }).await?
            }
            // One the app adopted from the environment, or a person set: the token the engine already takes. Keep it.
            Some(d) if d.managed_by.is_none() => r.keep(TOKEN, Rule { hosts: d.rule.hosts, ..rule }).await?,
            // The service's already.
            Some(_) => {}
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(256));
        for v in r.versions(TOKEN).await? {          // the current first, then the one before during the overlap
            bytes.extend_from_slice(&v.value);
            bytes.push(b'\n');
        }
        Ok(vec![File { name: "token".into(), bytes, secret: true }])
    }
}
```

and the engine (`engine/src/main.rs` in the same repository) reads `FERRUM_METRICS_ENGINE_TOKEN_FILE`, taking any of its
lines. The server's side carries the version the engine knows while the overlap lasts (`carry_at`,
[08](08-secrets-and-the-vault.md), *Rotation and overlap*). Comms does the same for LiveKit's `livekit.yaml` (each kept
version of `meet.secret` under the API key `ferrum.<version>`, `src/meet.rs`) and for the mail server's `admin-secret`
(one value, `src/hosted.rs`). Its `made_or_kept` (`src/keys.rs`) is the three-way match above as a function.

Keep files in `Zeroizing` buffers sized up front, so that no copy of a secret is left in memory a reallocation freed
(Comms, `src/meet.rs`, `ponytail:` comment).

### The services' own rules

- **Where it belongs.** `refusal` says why an installation may not have the service (the platform's bundled object
  store refuses any installation whose store is another one). A service that writes back a key its program made keeps it
  with `r.keep` and refuses where the vault's record (`r.vault().origin(k, name)`) does not say the program handed it over.
- **Everything is installation-scoped.** A service is the installation's program; `made`, `keep` and the render's reads
  are in `Scope::Installation`.
- **On the record.** Each secret given to a render is in `vault.event` as `render` (owner, secret, version, service; never
  the value), under the cap of a hundred an hour; a made secret as `make`, a rotation as `rotate` by `operator`.

### What the operator runs

| Task | What it does |
|---|---|
| `ferrum task services [format:json]` | Each service: owner, files, made secrets, and why it cannot be rendered here; never a value |
| `ferrum task render service:<name> to:<dir>` | Writes the files into `<dir>` from the database and the vault, with `DATABASE_URL` as its one input |
| `ferrum task vault_rotate level:service service:<name>` | New values of the secrets the vault made for the service, of the same kind, under the rule the service declares at that moment (its hosts from the settings as they are then); the ones before keep their rule and stay for their overlap |

The writer (`companion::write`) makes the directory when missing (never a link), refuses one that holds anything but an
earlier render of the same service (`.ferrum-render` says which), writes each file under a temporary name with
`O_EXCL` and renames it, removes what the earlier render wrote and this one does not, and lets renders into one
directory take turns. In Kubernetes an init container of the server's image renders into a memory-backed volume the
program mounts read-only (`/run/companion`); in Compose a one-shot service renders into a shared volume. The chart's
helpers are `ferrum.renderInit`, `ferrum.renderVolumes` and `ferrum.declaredServices`.

**A moved address means a rotation.** A render alone never changes where a secret may go: a changed settings row must
not redirect an existing secret. When the program's address moves (`comms.mail_url`, `metrics.engine_url`), the operator
stores the new address and rotates the service; the new secret is tied to the new host, and the old one never goes there
(`crates/kernel/src/companion.rs`; `crates/kernel/tests/companions_it.rs`, the address that moves).

**Rotation, for your operations notes.** A rotated secret reaches the program at its next start. Where the program holds
two values, rotate, restart the program within the overlap, and the server presents the version before until then
(Metrics, LiveKit). Where it holds one (the mail server's administrator password, `overlap_secs: 0`), restart it at once
after the rotation. Say which in the app's operations notes ([14](14-operations-for-apps.md)).

**What was run.** The kernel's `crates/kernel/tests/companions_it.rs` renders a meeting server declared as Comms
declares it, two replicas at once (one `make`), the refusals, a rotation and a closed vault, and starts LiveKit 1.13.11
on the written file with tokens the vault signs. `scripts/companions_rehearsal.sh` runs the real binary with
`DATABASE_URL` alone on the platform's bundled store. Not run: the chart, `deploy/k8s` and Compose (rendered and compared
only), and Stalwart (not on the machine; its file macro is from its documentation as remembered).

## Part 2: signing people in to the program

Ferrum is an OpenID Connect provider for the programs installed apps run (`crates/kernel/src/oidc.rs`). The program is
a confidential client its app declares in code, per workspace; there is no consent page and no dynamic registration
for it.

### As the app registers

```rust
// ferrum-app-cloud/src/lib.rs, inside hooks.app("cloud", …)
h.openid_companion(code::sign_in::NAME, Arc::new(code::sign_in::ForgeCompanion));   // "forge"
h.add_event_sink(Arc::new(code::sign_in::SignedOut));
```

The name is lowercase letters, digits and dashes, up to 40. Registered outside an app's registration, or twice, the
server does not start. The companion implements `oidc::Companion`:

```rust
// ferrum-app-cloud/src/code/sign_in.rs
#[async_trait]
impl ferrum_kernel::oidc::Companion for ForgeCompanion {
    /// Who is let in: a member with this right on an entity of the app's (some records are enough).
    fn right(&self) -> (&str, Action) {
        ("CloudForgeAccess", Action::Read)
    }

    /// What places the person at the program, beside the platform's claims. An error refuses the sign-in.
    async fn claims(&self, k: &Kernel, tenant: Uuid, user: Uuid) -> Result<Map<String, Value>> {
        let login = forge_login(tenant, user);
        signed_in(k, tenant, user).await?;
        Ok(json!({"preferred_username": login, "email": format!("{login}@people.ferrum.invalid")})
            .as_object().cloned().unwrap_or_default())
    }
}
```

- `right()` names an entity of the app's and an action. Anyone without it is told `access_denied`; once the app is gone
  from a workspace its entity is too, and so is everyone.
- `claims()` must never name one of the platform's claims (`iss`, `sub`, `aud`, `azp`, `exp`, `iat`, `nbf`, `jti`,
  `auth_time`, `nonce`, `acr`, `amr`, `at_hash`, `c_hash`, `sid`, `client_id`, `scope`): such a claim refuses the
  sign-in. The platform already gives `sub` (random per person and workspace, kept), `name` with scope `profile`, `email`
  and `email_verified` with scope `email`.
- `claims()` is called only while issuing that person a token for a live browser session of theirs, which makes it the
  place to let them back in at the program (below).

### For each workspace

The app's code declares the client with its own handle whenever it starts the program or its address changes, and
removes it when the program goes:

```rust
// ferrum-app-cloud/src/code/sign_in.rs, `publish`, in upkeep: for each workspace with Cloud, never a sandbox.
let client = v.companion(k, tenant, NAME, &[format!("{public}/user/oauth2/{name}/callback")]).await?;
let secret = v.reveal(k, Scope::Workspace(tenant), &client.secret).await?;   // to hand to the program
// … write the program's configuration with client.client_id and the secret
```

- `Vault::companion(k, tenant, name, &redirect_uris)` → `oidc::Client { client_id, tenant, app, name, redirect_uris,
  secret }`. One to ten return addresses, each `https://…` (or `http://` on this machine's loopback), with no fragment,
  matched exactly. Refused for a companion the app did not register: "app X registered no companion named …".
- The client's secret is the app's own secret `oidc-client.<name>` in that workspace, made by the vault once (32 random
  bytes), managed by the app (people's routes refuse to change it), revealed to the app's code to hand to the program.
  A sandbox has none.
- Declaring again changes nothing that is the same, from every replica at once (under a transaction lock).
- `Vault::remove_companion(k, tenant, name)` takes the client and its secret away; every sign-in through it ends, and
  the app is told.
- Skip a sandbox: `k.outward(tenant)` fails for one, and a sandbox answers nothing as an issuer.

The issuer is the address of the workspace the request came to (`https://<host>`), and discovery is at
`/.well-known/openid-configuration` there. Codes need PKCE (`S256`), live a minute and are good once; tokens are signed
`EdDSA` with the workspace's own key and live five minutes; there are no refresh tokens. Which hosts answer, and which do
not, is in [docs/api.md](../api.md), *Signing in to programs beside Ferrum*.

### When a sign-in ends: what the app owes

When the person signs out of that browser, signs out everywhere, changes their password, is deactivated, deprovisioned
or erased, the workspace is suspended, or the client is taken away, Ferrum stops answering for that sign-in at once and
publishes `ferrum.companion.signed_out` (`oidc::SIGNED_OUT_TOPIC`) through the outbox, within 15 seconds:

```json
{"user": "…", "subject": "…", "client_id": "fo_…", "app": "cloud", "companion": "forge", "session": "…",
 "reason": "signed_out | signed_out_everywhere | access_ended | expired | client_removed"}
```

**The program's own session is the app's to end.** Listen with an event sink, filter on your app and companion, and act
idempotently (events come at least once):

```rust
// ferrum-app-cloud/src/code/sign_in.rs
impl EventSink for SignedOut {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != ferrum_kernel::oidc::SIGNED_OUT_TOPIC || ev.payload["app"] != "cloud" || ev.payload["companion"] != NAME {
            return Ok(());
        }
        // … record a hold for (tenant, user) with the reason; the controller applies it at the forge once.
    }
}
```

Forgejo 17 has no call that ends one person's sessions, so Cloud locks the account (`prohibit_login`) on every reason,
keeps it locked for `access_ended`, and lifts it in `claims()` for the others, before the forge signs the person in
again ([docs/app-contract.md](../app-contract.md), *The forge, as Cloud configures it*). A program that can end one
session should end that session instead.

### Checklist for a companion

- The right is one a role in the app grants on purpose (Cloud: the roles Cloud User and Cloud Project Creator read
  `CloudForgeAccess`).
- The claims place the person and nothing else; an email claim is an address only that account has (the program may
  link by email).
- The program's own registration and account linking cannot let someone in as an account the app did not make for them.
- Every reason in the signed-out event is handled, and the handling is idempotent.
- The client is declared from upkeep (or as the program starts) for each workspace, never for a sandbox, and removed
  when the program goes.
- Rehearsed against the real program. The platform's `scripts/oidc_rehearsal.sh` does it for the forge with a stand-in
  for the Cloud app (`server/examples/oidc_rehearsal.rs`): a person signed in to a real Forgejo 17.0.1, then every way of
  leaving. Do the same for yours, and say in the README what was not run.
- Cloud's own: it writes each workspace's sign-in source in the forge's own database, and the forge makes and links no
  account by itself (`src/code/sign_in.rs`; [docs/app-contract.md](../app-contract.md), *The forge, as Cloud configures it*).

## Checked by

- `HookRegistry::settle` at every start: names, duplicates, declarations outside an app's registration.
- `crates/kernel/tests/companions_it.rs` (the `rust` job), `scripts/companions_rehearsal.sh` (`scripts/local_ci.sh
  companions`, run when named).
- `crates/kernel/tests/openid_it.rs`, `server/tests/oidc_it.rs`, `scripts/oidc_rehearsal.sh` (`scripts/local_ci.sh oidc`,
  run when named, with a real Forgejo).
- An app's own service and sign-in handling: its own suite (Comms' and Metrics' `tests/services_it.rs`); otherwise
  reviewed, not checked.

## See also

- [08 Secrets and the vault](08-secrets-and-the-vault.md), [07 Stateless apps](07-stateless-apps.md),
  [05 Permissions and people](05-permissions-and-people.md), [06 Events, jobs and logic](06-events-jobs-and-logic.md),
  [14 Operations for apps](14-operations-for-apps.md)
- [docs/app-contract.md](../app-contract.md); [docs/api.md](../api.md); [docs/operations.md](../operations.md);
  [docs/security.md](../security.md), *Programs beside Ferrum signing in through it*
