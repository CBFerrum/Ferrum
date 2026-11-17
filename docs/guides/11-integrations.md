# Integrations

How an app talks to the world outside it: calling other systems, being called by them, sending mail and messages,
syncing with a provider, using other apps' records, and carrying a country's or a sector's rules. In each case the
platform already has the part that is hard to get right (the guard on addresses, the credentials, retries, the
sandbox's wall); the app's job is to use it rather than build its own. The references are [docs/api.md](../api.md),
*Connections* and *Messages*; [docs/connectors.md](../connectors.md); [docs/plugins.md](../plugins.md);
[docs/plan/localization.md](../plan/localization.md).

## Which way to choose

| You need to | Use | Why not your own client |
|---|---|---|
| Call a described HTTP service a workspace configures (a carrier, a payment provider, a tax service) | A **connection** and `connections::call` | The workspace sees and edits it, its credentials are carried for you, retries, idempotency and the guard are the platform's |
| Call a service the operator runs for the whole installation (your engine, LiveKit's API) | `Kernel::vetted_client` (or `vetted_client_for(tenant, …)`) and `Vault::carry` | Pinned addresses, no redirects, a time limit, and the sandbox's wall |
| Be called by an outside service (a webhook) | A connection's **inbound** address (`map`, or your `InboundHandler`) | Verification, replay protection, a queue and a 202 at once |
| Sync records both ways with a provider (a CRM, a shop, an accounting system) | A **connector** (`connect::Connector`) | Cursors, conflicts, tombstones, dead letters, backoff per provider account |
| Send mail, a text, a chat message or an in-app notice | `messages::send` with a template in `package.yaml` | The workspace edits the words; opt-outs, quiet hours, restriction of processing, the record of each send |
| Let a person write a mail in the Mail app from your page | The `compose` intent | No dependency on Comms |
| Use another app's records | `requires` in `app.yaml` and the kernel's record calls | Its permissions, hooks and history apply |
| Make another app do something (take a payment, place a hold) | `uses` in the package and `Kernel::call_tx` | One gate: its hooks, rules and audit run, once per key, undone as a whole if it fails |
| Carry one country's or sector's rules | An **add-on** with data, on a general engine | Engines stay the same everywhere |

## Calling another system

### A connection

A connection is one described HTTP service: its base address, its authentication, its operations, their inputs and
results, its secrets ([docs/api.md](../api.md), *Connections*). No
provider is named in the platform's code: a provider is a description. An app ships the descriptions it needs in its
package (`connections:` in `package.yaml`), so that a workspace sees them offered and an administrator gives the secrets.

Calling one from an app (`connections::call`, `crates/kernel/src/connections/mod.rs`):

```rust
use ferrum_kernel::connections::{self, Call};

let result = connections::call(k, ctx, Some("erp"), &Call {
    connection: "carrier".into(),
    operation: "create_label".into(),
    inputs: json!({"weight": 2.5, "ref": doc.name}),
    key: Some(format!("label:{}", doc.id)),     // the same for every attempt at one change
    entity: Some("Delivery Note".into()),
    record: Some(doc.id),
}).await?;
```

- `app` is the calling package; the connection's description must list it in `apps`. `None` is a person's own call or
  the system's.
- `key` is required for an operation that changes something: a second call with the same key gets the first's result
  and calls nobody, and the service gets it in the operation's idempotency header.
- Errors: `RetryAfter` (with the seconds the service asked) or `Unavailable` for "try later"; `Invalid` for "the service
  said no". Queue a later attempt as a job ([06](06-events-jobs-and-logic.md)), never a loop in a request.
- Calls go to `https` only, to the base's host and `allowed_hosts`, never to a private, loopback, link-local or reserved
  address unless the operator names the host in `FERRUM_CONNECTION_HOSTS`.
- The credentials are the connection's secrets, carried by the platform (`vault::carried`), had only while the
  description still reaches the hosts they were given for. The app never sees them.

### The kernel's outbound client

For a service the operator runs for the whole installation, or one a connection cannot describe, use the kernel's
client (`crates/kernel/src/outbound.rs`):

```rust
let (client, url) = k.vetted_client_for(tenant, &address, /* allow_private */ false, Duration::from_secs(15), "ferrum-myapp").await?;
```

It refuses anything but `https` (plain `http` only with `allow_private`), resolves the host once and refuses a private
or loopback address unless `allow_private`, pins the request to the addresses it vetted (a DNS change between check and
connect cannot redirect it), follows no redirects, and sets your time limit and user agent. `vetted_client_for` first
asks `Kernel::outward(tenant)`, which refuses for a sandbox; `vetted_client` does the same for every workspace the
current work serves (an event's, a job's, a request's).

`allow_private: true` is for the operator's own services on a private network (Metrics' engine: "the engine is the
operator's, on a private network: its address may be private, and plain http"). Never pass it for an address a
workspace's person typed.

Put credentials on the request with `Vault::carry` ([08](08-secrets-and-the-vault.md)), so that a secret goes only to
the hosts its rule names.

**What is refused, and what the person is told.** Turn each refusal into a sentence that says what to do, and put the
detail in the log: Metrics answers "Metrics' engine cannot be reached" and logs the error; a `401` from the engine
becomes "Metrics' engine refused its token: the one Metrics holds … differs from the engine's"
(`ferrum-app-metrics/src/engine.rs`).

### What an app must not do when calling out

| Never | Why | What catches it |
|---|---|---|
| Build its own `reqwest::Client` for a call made for a workspace | It skips the address guard (server-side request forgery to the internal network) and the sandbox's wall: "An app that calls outside with an HTTP client of its own, not the kernel's, is not stopped by this" ([docs/operations.md](../operations.md), *A sandbox of a workspace*) | Reviewed, not checked |
| Call out inside a request while holding a database transaction | A slow service holds a connection and locks; connectors' rule: "A provider's call never holds a database transaction open" | Reviewed, not checked; the kernel's 10 s `lock_timeout` and 60 s idle-in-transaction limit cut it short |
| Wait on the network to save a record | A document never waits on a network call, except where a law requires clearance first ([docs/plan/localization.md](../plan/localization.md)) | Reviewed, not checked |
| Follow a redirect, or accept a private address from a person's input | Request forgery | The kernel's client refuses both |
| Put a credential in a URL or a log | URLs are logged | Reviewed; request logs keep the path without the query |

## Being called: webhooks

Point the outside service at a connection's inbound address, `POST /api/connections/inbound/{id}`, with `inbound` on
the description ([docs/api.md](../api.md), *Being called*):

- `verify`: a signature over the body (with a timestamp header and window, or with a delivery id so a body is taken
  once), or a token.
- `map`: the records it writes (find by, set, create), when it only writes records.
- `handler: "<app>"`: the app's code takes the payload instead. Register it:

```rust
// crates/kernel/src/connections/inbound.rs
#[async_trait]
pub trait InboundHandler: Send + Sync {
    async fn handle(&self, k: &Kernel, tx: &mut Tx, ctx: &Ctx, connection: &str, payload: &Value) -> Result<()>;
}
// in register:
hooks.add_inbound_handler("myapp", Arc::new(MyInbound));
```

The handler runs in the transaction that marks the call processed: what it changes commits with that, once. An error
that says the call cannot succeed marks it failed; any other is tried again. The caller is answered at once, whatever
the reason for a refusal (`401 this call was not accepted`); bodies up to 256 KB by default, 1 MB at most.

An app's own site (`/api/site/{tenant}/{package}/…`, `Site::handle`) can take a webhook too. Then the
app does what the inbound address does for free: verify the signature in constant time, refuse replays, rate-limit with
`Kernel::rate_limit`, keep the body small (`Site::max_body_for`), answer fast and do the work in a job. Prefer the
connection.

## Syncing with a provider: connectors

A connector translates one provider's model, credentials, events and limits into an app's records
([docs/connectors.md](../connectors.md)). Implement `connect::Connector` (`info`, `check`, `discover`, `pull`, `push`,
webhook authentication; `crates/kernel/src/connect.rs`) and register it with `HookRegistry::add_connector`, or ship
it as a plugin. The platform does the rest: credentials sealed and never read back, mapping per stream, a job per run
with a cursor, conflicts (`source`, `local`, `review`), tombstones, delivery with dead letters, webhooks authenticated and
deduplicated, reconciliation that repairs nothing by guesswork, and waiting together per provider account when the
provider says slow down. Describe what the provider's API really offers, not a common denominator.

## Mail, texts and notices

Every message an app sends to a person is a template the workspace can edit ([docs/api.md](../api.md), *Messages*):

```yaml
# package.yaml
messages:
  - key: bookings.confirmed
    label: Booking confirmed
    entity: Booking
    category: operational      # or promotional: opt-outs and quiet hours apply
    values: [link]
    languages:
      en:
        email: {subject: "Booking {{ doc.number }} confirmed", body: "<p>See you on {{ doc.starts_on }}.</p>"}
        text: {text: "Booking {{ doc.number }} confirmed for {{ doc.starts_on }}: {{ values.link }}"}
```

```rust
// in the transaction that makes the change
ferrum_kernel::messages::send(k, tx, ctx, &Message {
    template: "bookings.confirmed".into(), entity: Some("Booking".into()), record: Some(id),
    to: vec![Recipient::address(&email)],
    values: [("link".to_string(), json!(link))].into_iter().collect(),   // a serde_json::Map
    ..Default::default()
}).await?;
```

- It goes out only if the transaction commits. Leave `channels` empty: the template's order decides.
- Bodies are MiniJinja; an e-mail's body is HTML with every value escaped; secret fields are left out of `doc`.
- Nothing goes to a person whose processing is restricted, and marketing not to one who withdrew consent, under any
  address they have had: the platform asks `Kernel::may_contact_for` at the moment a message goes out. An app that
  reaches people another way asks it too ([12](12-privacy-and-data-duties.md)).
- A one-time code: `Kernel::send_code` and `Kernel::verify_code`, never a code table of your own.
- `Kernel::queue_mail` (`crates/kernel/src/outbox.rs`) queues a plain e-mail through the server's mailer after the
  commit, for wording no workspace edits; a message a person reads is a template.

Mail does not need the Mail app (Comms): the server's mailer sends it. When a person should write a mail themselves from
your page, start the `compose` intent and let whichever app handles it open its composer ([docs/plan/apps-ui.md](../plan/apps-ui.md),
§11.3 Intents): "A 'Send email' button in CRM needs neither a dependency on Comms nor a connector."

## Other apps' records

- **Say what you need.** `requires` in `app.yaml` names the apps and the version ranges ([03](03-the-manifest.md)).
  Depend on the least you need: `parties` for customers, `products` for items, ERP's packages only for their own
  records ([docs/plugins.md](../plugins.md), *Records other apps share*). Every package must work with only its
  dependencies installed: the catalog refuses links, overlays, workflows, seeds or personal-data rules that reach further.
- **Read and write through the kernel**, as a person would: the record calls, with the caller's context, so the other
  app's permissions, field groups, hooks, validation, numbering and history apply ([04](04-data-and-records.md)).
- **Never its tables.** An `update`, `insert into` or `delete from` of an `e_*` table outside the document pipeline is
  listed and given a kind by a person in docs/registry/mutation-paths.md; `scripts/mutation_paths.py` fails on one that
  is not ([04](04-data-and-records.md), *Writes outside the pipeline, and their review*). Another app's engine tables (journals,
  stock moves) are its own: ask it through its functions.
- **Its operations**, not only its records: an app offers one as an action with a contract, and the package that needs it
  says so in `package.yaml`, within what it depends on (`uses: [{capability: commerce.sales_invoice.request_payment,
  version: "^0.6"}]`: Bookings' `bookings_payments`). The app calls it in its own transaction with the vault handle its
  registration took, under a key of its own:

  ```rust
  // ferrum-app-bookings/src/payments.rs
  k.call_tx(&crate::keys::handle(k)?, ctx, tx, &Invocation::new(capability, input).key(key)).await?.result
  ```

  The call runs the owning app's operation as the system on the person's behalf, through the gate (its hooks, rules and
  audit apply), once per key. It is refused where no installed package of the caller declares it (`forbidden`) or the
  installed version is outside the range (`capability_unavailable`), is never left waiting for an approval, and when it
  fails it leaves nothing of itself in the caller's transaction, so the caller may carry on. The install screen lists
  the uses, and administrators see each app's calls (`GET /api/admin/apps/connections`). The full rules are
  [docs/app-contract.md](../app-contract.md), *Using another app's operations*; keep the handle inside the app's crate.
- **Hooks on its entities** register under `HookRegistry::scoped` (or inside your `app` registration) so they run only
  where your packages are installed.
- **Something that needs both sides** is a connector app: CRM's `crm-erp` (`requires: {crm, erp}`) adds ERP's taxes to
  quotations; its hooks are scoped to `crm_erp`.
- **Extension points**: an app may offer named points (`extension_points`) and contribute to those of the apps it
  requires (`contributes`), with a schema the installation plan checks ([docs/plugins.md](../plugins.md), *Extension
  points and add-ons*).
- **Across interfaces**: intents, slots and side panels, not shared code ([17](17-the-interface.md)).

## Country and sector rules: add-ons and data, never an engine's code

Engines are general. A country's or a sector's rules are data a workspace can see and change, or an add-on app on the
engine, never a branch on a country inside the engine ([docs/plan/localization.md](../plan/localization.md)):

- **An add-on** is its own app with `parent: <engine app>` and `requires: {<engine app>: …}`, in the engine app's
  repository under `addons/<code>` (ERP's: `ca`, `countries`, `de`, `eu`, `eu-members`, `gb`, `india`, `us` and more), its
  fields prefixed with its code (`in_gstin`), registered only where its package is installed and only for companies of
  its country, so one workspace can keep companies in several countries.
- **The seam** is the engine's (ERP's `Localization`: `determine` which taxes apply, `check` before submit,
  `setup_company`), with document hooks for the rest.
- **Rules as data** where they are data: tax rates, templates, codes, calendars, print layouts, message templates.
- **Talking to an authority** (e-invoicing, returns): a hook queues the work in the outbox, an event sink talks to the
  authority with retries and an idempotency key, and the answer lands in the add-on's fields. Credentials are secret
  fields of the add-on's, read with its handle ([08](08-secrets-and-the-vault.md)), or a connection; sandbox and
  production endpoints a per-company setting defaulting to the sandbox.
- **Tested** end to end with a fake authority, and against the authority's sandbox where it has one; what was never run
  against the real authority is said so.

Before you build an integration, look for a mature open-source project to run or depend on (AGPL-compatible only), and
read `docs/references/` for what was already compared ([13](13-security-rules.md), *Dependencies and licences*).

## Plugs

"Plugs" is a word of the Social app's (its feature), not a platform term: there is no `plug` in the kernel or the
manifest. For extending another app, see *Extension points* above.

## Checked by

- The kernel's guard on every connection call and outbound client (`crates/kernel/src/outbound.rs`,
  `crates/kernel/src/connections/`), and the sandbox's wall (`Kernel::outward`).
- `scripts/mutation_paths.py`: direct writes to `e_*` tables.
- `crates/meta/src/catalog.rs` (a `uses` of a package it does not depend on, or a version that is no range) and
  `crates/kernel/tests/app_uses_it.rs` (`Kernel::call_tx`: declared, in range, once per key, undone when refused).
- The catalog (`crates/meta/src/catalog.rs`): a package reaching past its dependencies is refused.
- `compiled_apps_fit_together` (`server/src/packages.rs`): what a compiled app requires of another, the release compiled
  with it meets; contributions meet their point's schema.
- An app's own HTTP client, network calls inside requests, country branches in engine code: reviewed, not checked.

## See also

- [06 Events, jobs and logic](06-events-jobs-and-logic.md), [08 Secrets and the vault](08-secrets-and-the-vault.md),
  [12 Privacy and data duties](12-privacy-and-data-duties.md), [13 Security rules](13-security-rules.md),
  [19 Languages and formats](19-languages-and-formats.md), [21 APIs and docs](21-apis-and-docs.md)
- [docs/api.md](../api.md), *Connections*, *Messages*, *App endpoints*; [docs/connectors.md](../connectors.md);
  [docs/plugins.md](../plugins.md); [docs/plan/localization.md](../plan/localization.md)
