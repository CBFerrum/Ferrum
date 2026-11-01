# Applications, integrations and extensibility

The living plan for turning what Ferrum already has into a business platform people can rely on. It
answers the strategy assessment of 1 November 2026, and keeps its decisions here rather than in a
conversation. The registry (`docs/registry/`) tracks each requirement's status.

## What the assessment found, against the code

The assessment read an earlier commit. These are its findings as the code stands.

| Finding | Status | Where |
|---|---|---|
| `http.fetch` reachable from transactional hooks | Closed. Refused in hooks, with `sockets.*`; work leaves through an event after the commit | `crates/plugins/src/host.rs`, `after_commit_it` |
| Budgets need a wall-clock dimension | Closed. Deadlines cover host calls (15 s in a hook, 300 s otherwise), a call in progress is dropped, workspaces have a limit on running calls | `crates/plugins/src/runtime.rs` |
| Credentials enter plugin configuration | Closed for what a plugin declares. Declared credentials stay out of its input; the host signs requests in, only for their declared hosts, and checks signatures. Stripe, Razorpay and PayPal have moved | `crates/plugins/src/host.rs`, `docs/plugins.md` |
| Outbox: batches outliving their lease | Closed. It was real: ten events at 120 s against a 600 s lease. Each event now takes the lease afresh and finishes only under the lease it took; retries spread out | `crates/kernel/src/outbox.rs`, `events_it` |
| Logger cuts a string by byte index | Closed. It was worse than a garbled line: a plugin chooses that text, so it could panic the host | `crates/plugins/src/host.rs` |
| Stripe exceptions logged for a manual refund | Closed, in all three gateways. A Payment Exception a manager sees, a reviewed refund the gateway carries out once. A gateway can never approve one | `ferrum-app-commerce` |
| Generic HTTP unsuited to large media | Open. Needs streaming workers and object-storage references when a connector needs them | |
| SDK packages not on npm | Open. `@ferrum/*` still reach into `web/src`, so publishing means packaging them properly first, and needs the owner's npm account | |
| Two plugin SDKs | Found here, closed. Ferrum carried a drifted copy; `ferrum-sdk` is now the only one. It could not be used as a git dependency at all, because of a placeholder in a template, so nothing scaffolded could build. Fixed, and CI builds one | `ferrum-sdk` |

## The connector contract

The assessment's contract, and where each part is:

| Operation | Here |
|---|---|
| `describe_capabilities` | `Connector::info`: streams, writes, webhooks, API version, requirements, capabilities |
| `validate_connection` | `Connector::check` |
| `discover_schema` | `Connector::discover` |
| `read_changes` | `Connector::pull`, paged, with a durable cursor and tombstones |
| `apply_command` | `Connector::push`, with a key that is the same for every attempt |
| `verify_webhook` | `Connector::webhook`, then deduplicated by event id |
| `reconcile` | The kernel's, on top of `pull`: records differences, repairs nothing by guesswork |
| `health` | The kernel's: authentication, freshness, backlog, dead letters, drift and quota, told apart |

Its records map onto what exists: `Connection` is `connection`; `CredentialReference` is its sealed
column; `ExternalObjectMap` is `connection_link`; `SyncCursor` is `connection.cursors`;
`IntegrationJob` is the job queue; `InboxEvent` is `connection_event`; `SyncConflict` and
`ReconciliationResult` are `connection_drift`; `ConnectorHealth` is `health`.

Not built: token refresh in a broker for OAuth providers (PayPal's short-lived token still lives in
its plugin's store), provider-wide budgets shared between connections, and a reviewer's screen for
any of this. The operations exist as API; the connection console is a screen still to be drawn.

## Admission gates

A connector enters committed development only with a real customer workflow, a permitted production
access route, a test environment, a maintenance owner and acceptable operating economics. An official
API is not an approval: Booking.com's connectivity site says it is pausing new providers, for one.
Each connector publishes its capabilities, authentication, approved scopes, production approval,
quotas, commercial terms, data restrictions, supported regions and maintenance owner.

**Ferrum has no certified or real-provider connector today.** The machinery is tested against
stand-ins, Stripe's and PayPal's mocks, and nothing against a live provider. Keys and approvals are
the owner's to obtain.

## Certification levels

Proposed, not in force: nothing has been certified. A level attaches to a specific version, capability
set, configuration and provider API version, never to a name.

| Level | Means | Takes |
|---|---|---|
| Community | An identified publisher and a valid package, and no endorsement | Signature, declared permissions, licence, documentation, automated checks |
| Verified | A reviewed security boundary and a tested contract | Tenant isolation tests, credential and logging review, failure tests, a compatibility matrix, a named maintainer |
| Business-Critical | Fit for the stated critical workflows and configurations | Operational ownership, recovery exercises, reconciliation, incident response, supported versions, recertification |

Disabling a connector must not silently bypass a financial or compliance rule: the right outcome may
be to pause the workflow and say why.

## Order of work

1. Finish the operating layer: the console for connections and exceptions, OAuth in a broker, media by
   reference.
2. Certify what exists: Stripe, Razorpay and PayPal against their sandboxes, once keys exist.
3. Publish the SDK and its compatibility matrix, then have outside developers try it on real tasks.
4. Build a connector only through the gates above. Services, Retail and Field Operations packages come
   before any new vertical; hospitality needs its own domain model and an approved channel manager,
   never appointments and iCal.
