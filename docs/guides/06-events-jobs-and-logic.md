# Events, jobs and logic

What an app does after a change, in the background, over hours, and for the builder, processes, the assistant and
agents. The contract for events is [docs/events.md](../events.md); for capabilities
[docs/logic/capabilities.md](../logic/capabilities.md); for durable processes
[docs/logic/processes.md](../logic/processes.md). This guide is how an app keeps to them.

## The one rule: nothing leaves from inside a transaction

A document hook runs inside the save's transaction, which may still roll back. So a hook reads and writes the
database, emits events and queues mail, and never calls anything outside: no HTTP request, no mail server, no
socket, no other process ([docs/events.md](../events.md), *What may happen where*).

| Where | May | May not |
|---|---|---|
| A document hook, native or plugin | read and write in the document's transaction, emit events, queue mail | call anything outside. Plugins are refused (`http.fetch`, `sockets.*`); native hooks are held to it by review |
| An event handler, a job, upkeep, an endpoint | call out, in short transactions of their own | assume they run once |

Decide in the hook, emit the work as an event or a job in the same transaction, and do it after the commit: the
event or job exists only if the change committed. `plugins/examples/after-commit` is this shape for a plugin.

What else never goes in a request:

- **Work that must survive a restart, or takes longer than a request** (an export, a send to a list, a sync, a large
  recalculation): a job.
- **A transaction held open across a provider's answer or a person's decision.** A long process that crosses systems
  is a chain of events with its own state and a way to compensate (a saga, a process).
- **Waiting by sleeping or polling** in a handler: a job returns `Error::Wait` and comes back later.
- **A task spawned on its own** (`tokio::spawn`) for work that matters: it starts without the request's trace or
  zone, nothing retries it, and a restart loses it. Use a job, or run it under a `Trace` scope when its events matter
  ([docs/events.md](../events.md), first section).
- **A loop over every workspace or every record in one transaction**: upkeep goes workspace by workspace in a
  transaction each (GRC's `maintenance_on`); a release's data change is a backfill in batches
  ([04-data-and-records.md](04-data-and-records.md)).

## Events

### What the platform raises

Every record operation writes an event in the outbox, in its own transaction (`crates/kernel/src/outbox.rs`): topic
`{Entity}.{created,updated,submitted,cancelled,deleted}`, `{Entity}.transitioned` for a move within one document
status, `{Entity}.emailed`, `plugin.{plugin}.{topic}` for a plugin's, and `ferrum.*` for the platform's own. An event
names its record (`entity`, `record_id`, `source_version`), who made the change (`actor`), the chain it belongs to
(`correlation_id`, `causation_id`, `traceparent`) and its `priority`. The table of columns is in
[docs/events.md](../events.md), *What an event says*.

Platform topics an app may need to hear:

| Topic | Payload | What an app does |
|---|---|---|
| `ferrum.user.deprovisioned` | `{user, email, reassign_to, reason}` | hands over what its `handover` declaration cannot express (Bookings marks the person's staff record inactive); it may fire again when a successor is named later |
| `ferrum.privacy.erased` | `{account, records: [{entity, id, fields}], reason}` | nothing for declared data; see [12-privacy-and-data-duties.md](12-privacy-and-data-duties.md) |
| `ferrum.companion.signed_out` | `{user, subject, client_id, app, companion, session, reason}` | ends the companion's own session ([10-companions-and-sign-in.md](10-companions-and-sign-in.md)) |

### Publishing an app's own

Declare a business event in `package.yaml` so that processes can start on it and wait for it (`EventDef` in
`crates/meta/src/types.rs`):

```yaml
events:
  - name: visit_done                 # [a-z][a-z0-9_.]*, unique in the package; topic visits.visit_done
    label: Visit done
    entity: SiteVisit                # when it is about a record
    payload: {type: object, required: [visit], properties: {visit: {type: string}}}   # its JSON Schema
```

Write it in the transaction of the change, so it exists only if the change commits:

```rust
// In the hook's or the action's transaction (crates/kernel/src/outbox.rs):
ferrum_kernel::outbox::enqueue(hc.tx, hc.ctx.tenant, "visits.visit_done", Some("SiteVisit"), Some(visit), json!({"visit": visit})).await?;
// Or, saying who made the change and which version it made:
ferrum_kernel::outbox::enqueue_change(tx, tenant, "visits.visit_done", Source { entity: "SiteVisit", record_id: visit, actor: Some(user), legal_entity: None, version: Some(v) }, payload).await?;
```

A payload carries ids and what a consumer needs to decide, never a secret and never more personal data than it needs:
events stay in the table for a while and dead ones are shown to administrators (without payloads).

Bulk work (a send to a whole list, an import) runs under `Trace::at_priority(PRIORITY_BULK)` so it stands behind
what people are waiting for, such as a password reset (`ferrum_kernel::outbox::PRIORITY_BULK`; *Fairness* in
[docs/events.md](../events.md)).

### Hearing events

An app hears events through an event sink it registers (`EventSink` in `crates/kernel/src/hooks.rs`):

```rust
pub fn register(hooks: &mut HookRegistry) {
    hooks.add_event_sink(Arc::new(Departures));
}

#[async_trait]
impl EventSink for Departures {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != DEPROVISIONED_TOPIC || k.registry.schema(ev.tenant_id).await?.entity("BookingStaff").is_none() {
            return Ok(()); // not ours, or the workspace lacks the app's package
        }
        // … a short transaction of its own
    }
}
```

(Bookings' `src/departures.rs`.) A sink is called only for the workspaces that installed one of its app's packages, as
the app's document hooks run (the platform's own sinks, and an app's registered without packages, hear every
workspace), so its own check of the package is no longer needed; it filters by topic first.

**At least once, so the first time is asked.** A worker leases a batch for 30 seconds; if it dies, another replica takes the events
again. There is no exactly-once ([docs/events.md](../events.md), *Delivery*). A handler is either naturally idempotent
(Bookings' `update … set active = false … where … and active`), or asks, in the transaction that does the work,
whether it is the first time:

```rust
// CRM's src/filing.rs: filing a hosted mailbox's message, once per message.
let mut tx = k.db.system_tenant_tx(tenant).await?;
if !ferrum_kernel::outbox::first_time(&mut tx, tenant, "crm.filing.mailbox", ev.id).await? {
    return Ok(());
}
// … the work, in the same transaction …
tx.commit().await?;
```

`first_time(tx, tenant, consumer, event_id)` inserts into `event_receipt`: a repeat, even one arriving at the same
moment, waits for the first to commit and is told no; if the first rolled back, the repeat is told yes. Name the
consumer `<app>.<what>`.

**Order** holds within a workspace and a priority until something fails: a retry goes behind later events, and during
an upgrade an older replica leaves a newer build's event for a newer one. Anything that depends on order compares
`source_version`, never arrival.

**Failing.** An error makes the event retry for this sink alone: 10 s, 20 s, 40 s and so on up to an hour, dead after
15 attempts; the other sinks and automations are not stopped by it, and those that handled the event are not called
again. One event's handlers get 120 seconds. Dead events stay for a person (`GET /api/admin/events/dead`,
Administration's *Needs a person*) and can be replayed; consumers that already handled it are not repeated. A sink is
recorded under `{app}.{what}`, `what` being `EventSink::name` (its type's name unless it says otherwise; two sinks of
one app need two names). Return an error only for
what a retry can fix; an event that can never be handled is logged and returned `Ok`, or it blocks nothing but fills
the dead list.

## Jobs

A job is durable background work: longer than a request, or something that must survive a restart
(`crates/kernel/src/jobs.rs`). Any replica runs it under a lease it renews every 5 seconds (60-second lease); a
replica that dies leaves the lease to run out and another takes over; each claim is an attempt, so a job that takes
its worker down is given up on, not tried for ever.

```rust
pub struct Export;

#[async_trait]
impl JobHandler for Export {
    fn kind(&self) -> &'static str { "visits.export" }
    fn title(&self) -> &'static str { "Visits export" }        // what people are told it is
    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let ctx = job.creator(k).await?;                       // as the person who asked, loaded now
        // … the work; job.resumed says how far an earlier attempt got; job.progress(k, json!(…)) reports it;
        // job.cancel_requested(k) says whether to stop …
        Ok(json!({"rows": n}))
    }
    // fn timeout(&self) -> Duration: one attempt's longest (an hour by default)
}

// Registered with the app:
hooks.add_job_handler(Arc::new(Export));
// Queued in the request's transaction, so it exists only if that commits:
let mut job = NewJob::new("visits.export", json!({"filters": filters}));
job.dedupe_key = Some(format!("export:{user}"));          // one already queued or running is returned instead
ferrum_kernel::jobs::enqueue(&mut tx, tenant, Some(user), job).await?;
```

| Behaviour | How |
|---|---|
| Runs as the person who asked, with their rights as they are now | `Job::creator`; `Job::system` for the engine's own |
| Safe to repeat | it runs at least once; `Job::resumed` carries what an earlier attempt recorded |
| Cannot succeed (invalid, forbidden, not found) | return that error: the job ends at once, `failed`, kept for a person |
| Try again later without using an attempt | `Error::Wait(message, seconds)` |
| A service said when (`Retry-After`) | `Error::RetryAfter(message, seconds)`: it costs an attempt |
| Asked to stop | a queued job is dropped; a running one's work is dropped at its next `await`, so its open transaction rolls back; with `undo_on_cancel` it is run to undo instead |
| Fair | each workspace gets its turn; four jobs a replica at once |

`POST /api/jobs/{id}/cancel` and *Needs a person* are what administrators see ([docs/api.md](../api.md),
*Background jobs*). A job tried again runs with the rights its requester has now: someone who has left gets nothing
from it.

## Schedules and upkeep

An app's `maintenance(&Kernel)` runs every 60 seconds (`EVERY` in `server/src/packages.rs`). Each app's upkeep holds an
advisory lock of its own while it runs, so one replica runs it at a time and the others leave it; a replica runs two
apps' upkeep at once (`FERRUM_UPKEEP_CONCURRENCY`), in an order that differs between replicas. A failure or a panic is
logged with the app's name and stops nothing; a round longer than the interval is warned about.

So upkeep:

- **Goes workspace by workspace**, each in a transaction of its own, and skips workspaces without the app's package.
  GRC's `maintenance_on` lists active workspaces in a system transaction, commits it, then opens `tenant_tx` for each
  that has `grc` installed, and keeps the first error to return after the rest ran.
- **Takes the day as a parameter** (`maintenance_on(k, today)`), so the scenario test can run it at a chosen date.
- **Is cheap when there is nothing to do**, and idempotent: it runs every minute, and twice is the same as once.
- **Calls nothing outside for a long time**: it holds a database connection while it runs. Work that does is a job
  upkeep queues (with a `dedupe_key`).

Other schedules: a package's automation with `event: scheduled` and a `schedule` (a workspace may edit it); a plugin's
`schedules` (at least every 60 seconds, one replica at a time); a process's `delay` step
([docs/plugins.md](../plugins.md), [docs/logic/processes.md](../logic/processes.md)).

## Work that crosses systems

### Sagas

`ferrum_kernel::saga` makes a chain of outside steps a durable job: steps in order, each doing one thing outside and
knowing how to undo it ([docs/events.md](../events.md), *Workflows that cross systems*; `crates/kernel/src/saga.rs`).

```rust
struct Reserve;

#[async_trait]
impl Step for Reserve {
    fn name(&self) -> &'static str { "reserve" }
    async fn run(&self, k: &Kernel, cx: &StepCx<'_>) -> Result<Value> {
        // send cx.key as the provider's Idempotency-Key: it is the same every time this step runs for this workflow
        …
    }
    async fn undo(&self, k: &Kernel, cx: &StepCx<'_>, done: &Value) -> Result<()> { … }
}

hooks.add_job_handler(Arc::new(Saga::new("order", "Order", vec![Arc::new(Reserve), Arc::new(Charge)])));
Saga::start(tx, tenant, Some(user), "order", input, Some(dedupe)).await?;   // in the request's transaction
```

It records what each step gave back, so a retry, a restart or another replica resumes at the step under way; later
steps read earlier ones' answers (`cx.output("reserve")`). A cannot-succeed error turns it around at once; others are
tried again, ten tries forward, then it turns around. Turned around, it undoes what was done, newest first. An undo
that cannot be done stops it, naming what a person must still undo. Cancelling waits for the next step boundary,
undoes, and ends `cancelled`.

### Outcomes that cannot be known

A call whose answer was lost may or may not have happened. The capability contract says what then happens
(`UnknownOutcome` in `crates/logic/src/capability.rs`): `reconcile` (the capability can look the outcome up),
`manual` (a person settles it, with evidence), or `not_applicable`. A process step that can end unknown says where
that goes (`on_unknown`), and no route from an unknown outcome leads to a completed end
([docs/logic/processes.md](../logic/processes.md), *Steps*).

What an app does: send a stable key on every outside call that changes something (`cx.key`, a plugin's
`operation_id`, a capability's effect key), so the provider can recognise a repeat; where the provider has no such key,
ask what it already did before asking again (`attempt` above 1 in a plugin's input says an earlier run may have done
it: [docs/plugins.md](../plugins.md), *At least once*); and give an action that reaches outside a contract saying so
(below). A plugin's `sockets.send` succeeding means queued, not done: anything with a physical effect belongs on the
command ledger ([docs/plan/industrial.md](../plan/industrial.md)).

### Durable processes

Processes run for hours or months: they wait for people, events and deadlines and do their work through capabilities
([docs/logic/processes.md](../logic/processes.md)). They run on Temporal where the server has the `process` feature and
`TEMPORAL_ADDRESS`; without it, they do not run. An app does not write processes; it gives them what they need: actions
with contracts, events declared, and compensating actions.

## Idempotency

| Where | Key | What a repeat gets |
|---|---|---|
| `POST /api/v1/{E}` | `Idempotency-Key` header, 8 to 200 characters | the original record, `200`, `Idempotent-Replayed: true`; with `?submit=true` a replay never submits twice |
| A capability through the gate | an effect key, the same for every attempt at one effect | what was done, `repeated: true`; other input under the same key is refused `input_changed` (409) |
| An event handler | `first_time(tx, tenant, consumer, event_id)` | told no |
| A job | `NewJob::dedupe_key` | the job already queued or running |
| A saga step, a plugin's event | `cx.key`; `operation_id` | the provider's own deduplication |
| An app's own endpoint that creates | read `Idempotency-Key` and keep it with what was made, in the same transaction | reviewed; how is in [21-apis-and-docs.md](21-apis-and-docs.md) |

A result given once (a new card's code) is `contract.answer_once: {fields, then}`: a repeat under the same key gets the
rest of the answer and is told the code was given in the first answer only (`AnswerOnce` in
`crates/meta/src/types.rs`).

## What the logic platform, the builder, the assistant and MCP need from an app

They all work on what the app declares, through the same gate as people, so an app gives them the most by declaring
well ([docs/logic/capabilities.md](../logic/capabilities.md)).

**Capabilities come from what is installed.** Every entity gives `read` and `list`; every entity but a ledger
`create`, `update`, `delete`, and `submit` and `cancel` when submittable; each action its package declares gives
`{action}`; each workflow transition `transition.{action}`. An id is `{package}.{entity}.{operation}`
(`grc.policy.publish`), versioned with its package. An action marked `hidden` is left out unless the app writes it a
contract. The record operations' names (`create`, `read`, `list`, `update`, `delete`, `submit`, `cancel`, `amend`)
cannot be action names.

**An app uses another app's operation** through the same gate, in its own transaction: it declares the capability in
its package's `uses`, and calls it with `Kernel::call_tx` and the vault handle its registration took, under an effect key
of its own ([11-integrations.md](11-integrations.md), *Other apps' records*).

**A contract** says what an action promises (`ActionContract` in `crates/meta/src/types.rs`); everything is derived
when left out, and a contract that contradicts its action is refused when the package loads:

```yaml
actions:
  - name: mark_done
    label: Mark It Done
    permission: write
    contract:
      completion: The visit is recorded as done      # what "done" means
      compensate_with: reopen                        # another action of the entity that undoes it
      returns: {type: object, properties: {status: {type: string}}}
      # effect: pure | query | transactional_action | external_effect | operational_intent | human_task
      # idempotency: pure | read_only | native_receipt | provider_key | reconcile_only
      # unknown_outcome: not_applicable | reconcile | manual
      # retention_days, answer_once, deprecated: "mark_done_v2 replaces it from 1 March 2027"
```

Give an action that reaches outside (sends, pays, calls a provider) `effect: external_effect` and its
`unknown_outcome`; give one that only reads `read_only: true` so it is a query. A link in what an action takes or
returns names its entity (`"x-ferrum-link": "SiteVisit"`), as link fields do.

**Events for processes**: each entity's own and the business events in `events:` are what a process can start on and
wait for.

**Before an app is taken out**, what runs on its capabilities is listed (`GET /api/app/studio/dependents?package=`)
and its steps then wait for it. Deprecate an action (`contract.deprecated`) before removing it.

**The builder** (Studio) configures on the same metadata: a workspace's own fields, rules, actions with effects,
workflows, approvals, automations and published lists ([docs/builder/README.md](../builder/README.md),
[04-data-and-records.md](04-data-and-records.md)). An app keeps its rules in metadata where it can (a rule, a
`condition`, an approval policy) rather than in a hook, so a workspace can see and change them.

**The assistant and MCP** use the same tools, as the person, within their permissions; every change is shown to the
person before it runs ([docs/operations.md](../operations.md), *The assistant*; [docs/api.md](../api.md), *AI agents
(MCP)*). An app helps them by:

- describing each entity (`description`) and module, which the help center and the assistant read;
- `shell.assistant` in the manifest: the app's own words about its records (states, what counts, where figures come
  from) and up to six questions to suggest, never a person's data ([03-the-manifest.md](03-the-manifest.md); GRC's
  `app.yaml` is an example);
- marking pay, health and personnel entities `internal`, and sensitive fields in field groups, so they do not leave
  for a model unless the workspace says so (`Egress` in `crates/kernel/src/ctx.rs`);
- giving actions labels, parameters with descriptions, and contracts.

## Checked by

- `crates/kernel/src/outbox.rs`, `crates/kernel/src/jobs.rs`, `crates/kernel/src/saga.rs` and their tests: delivery,
  leases, retries, `first_time`, saga resumption and undo.
- `crates/meta` (package load): event declarations, action contracts that contradict their action, reserved action
  names.
- `crates/studio/tests/catalog_it.rs`: the builder, the API and MCP describe a capability the same way.
- `scripts/mutants.py`: mutants on the outbox, jobs and the gate, run with the platform's tests.
- The app's scenario test: upkeep at a chosen day, a handler run twice doing the work once.
- Reviewed, not checked: no outside call in a native hook; no spawned tasks for work that matters; upkeep per
  workspace; keys sent on outside calls.

## See also

- [04-data-and-records.md](04-data-and-records.md): hooks, actions, backfills.
- [05-permissions-and-people.md](05-permissions-and-people.md): who a job and a process act as.
- [07-stateless-apps.md](07-stateless-apps.md): several replicas at once, locks and leases.
- [11-integrations.md](11-integrations.md): calling other systems and receiving webhooks.
- [docs/events.md](../events.md), [docs/logic/capabilities.md](../logic/capabilities.md),
  [docs/logic/processes.md](../logic/processes.md), [docs/plugins.md](../plugins.md).
