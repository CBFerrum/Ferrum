# Events

What happens after a transaction commits: an event in `outbox`, written in that transaction, handled
by any replica. This is the contract for everything that reacts to a change: webhooks, automations,
plugins, apps' event sinks, mail and push.

## What an event says

| Column | Meaning |
|---|---|
| `id` | The event's identity, increasing with time. Consumers deduplicate on it. |
| `tenant_id` | The workspace. |
| `topic` | `{Entity}.{created,updated,submitted,cancelled,deleted}`, `{Entity}.transitioned` (a workflow move within one document status), `{Entity}.opened` (a signed action link was used for the first time), `{Entity}.scheduled` (a scheduled automation's moment), `{Entity}.merged`, `{Entity}.emailed` (the record was mailed from its form: to whom, and whether its PDF went along), `audit.{category}` (audit events, which only webhooks that name them receive), `plugin.{plugin}.{topic}`, or `ferrum.*` for the platform's own. |
| `entity`, `record_id`, `source_version` | The record it is about, and the version that change made. |
| `actor` | Who made the change; empty for background work. |
| `legal_entity` | The record's company, when it has one. |
| `correlation_id` | The request or job that started the chain: the request id in the server's log, so one search finds a request's log lines and all its events. |
| `causation_id` | The event this one answers. Empty for the first of a chain. |
| `traceparent` | The caller's W3C trace context, carried unchanged (an invalid one is dropped). |
| `schema_version` | The payload's shape; 1 today. |
| `priority` | 0 urgent, 1 normal, 2 bulk. |
| `created_at`, `available_at`, `processed_at` | When it was written, when it may next run, when it was handled. |

The trace columns need no code at the call site: they default to what the writing transaction was
given. `Trace::scope` sets them for a stretch of work. The server does that for each request, and
the dispatcher for each event it handles, so a handler's own events name the event that caused them.
Tasks a handler spawns start without a trace; run them under one if their events matter.

## Delivery

**At least once.** A worker takes a lease on a batch (30 seconds), takes each event again as it begins
it (for its handlers' 120 seconds and 30 more), and handles it outside any long transaction. If the worker
dies, what it had not begun passes to another replica within 30 seconds, and the event it was at when its
own lease ends; a replica that is stopping finishes what it has begun and hands back the rest of its batch at
once. Webhook deliveries are
unique per webhook and event, plugin runs per plugin and event, automation runs per automation and event (a
scheduled automation's moments per automation, record and moment), and what an event sink handled is
recorded (below). There is no exactly-once, and nothing should assume it.

**What one event goes through, in order:** webhook deliveries are queued for the subscribed webhooks
(an audit event only for those that name its topic, never for `*`; an app's webhook gets which record,
never `doc` or `was`), followers are told (first attempt only), automations run, then each registered
event sink is called, in registration order (plugin subscriptions are one): an app's sinks only in the
workspaces that installed one of its packages, as its document hooks run. Each automation and each sink
has its own outcome: one that fails does not stop the others, and the event is tried again for what failed
alone. An automation run that can never succeed (it creates an entity no longer installed, the record lacks a
field it needs: an error that says invalid, not found or forbidden) is recorded as failed, saying why, and
does not fail the event; an automation that ran is not run again for the event. A sink that handled the event
is not called for it again when the event is retried or replayed: when an attempt fails or runs out of time,
the sinks that handled it are recorded in `event_receipt` under their names (`{app}.{what}`, from
`EventSink::name`). Only a replica that dies while handling an event can call again a sink that had finished,
so a sink whose work must happen once asks `outbox::first_time` in the transaction that does it. An event
whose record was erased while it waited carries nothing of the record any more (it says `erased`): it reaches
no webhook, follower, automation or sink, and counts as handled.

**Order** holds within a workspace and a priority, until something fails: a retry goes behind later
events. During an upgrade or after a rollback, an event written by a newer build can be delivered after later events of
the same workspace (an older server leaves it for a newer one, docs/operations.md); `source_version` covers that too.
Anything that depends on order compares `source_version`.

**Fairness.** A batch gives each workspace with work at most 10 events, of 50, and handles workspaces
side by side. So one workspace's backlog, or its slow provider, does not delay another's events. Side by
side means as many events at once as a quarter of the replica's database pool (7 of the default 30), shared
with the webhook sender's reads, so that a backlog never takes the connections requests need; slow providers
holding all of those at once delay the rest by up to their handlers' 120 seconds.
Within a workspace, mail and push go first and bulk work last: bulk operations, imports, jobs at
bulk priority and sends to a whole list. A mail sent under bulk priority stays there, so 50,000
campaign messages do not stand in front of someone's password reset. Run anything of the kind under
`Trace::at_priority(PRIORITY_BULK)`.

**Retries.** A failed event waits 10 s, 20 s, 40 s and so on, up to an hour (each a little shorter by
chance, so what failed together does not return together), and is dead after 15 attempts. One event's
handlers get 120 seconds before they count as failed.

**Chains.** An event written while handling another is one step further from the change that started
the chain. An event 8 steps deep, or the 1,001st from one change, is not handled: it is dead at once, its
last error naming the chain, for whoever fixes the rule, automation, plugin or app that answers its own
changes and then replays it. A request's or job's own events never count: an import of ten thousand rows
is one step.

**Webhooks** are delivered apart from the event, 20 at a time, each its own request: `POST` of the
payload (at most 256 KiB, or the delivery fails) with `X-Ferrum-Event` (the topic), `X-Ferrum-Delivery`
(the delivery's id, the same on every attempt) and `X-Ferrum-Signature: t=<unix seconds>,v1=<hex
HMAC-SHA256 of '<t>.<body>' with the webhook's secret>` ([how to check it](operations.md#sending-the-audit-log-elsewhere)).
Any answer but 2xx fails the attempt. A request that takes more than 10 seconds fails, redirects are not
followed, and an address that resolves to a private one is refused unless the operator allows it. A
failed delivery is tried again after 1 minute, 2, 4 and so on up to 6 hours, and is dead after 12
attempts. A sandbox sends nothing: its deliveries are dead at once, saying why.

**Dead events** stay in the table for a person to look at; the `ferrum_outbox_dead` and
`ferrum_webhook_dead` metrics count them. Handled events and delivered webhooks are deleted after 30
days, and with them the receipts of what consumers handled; a waiting or dead event keeps its receipts however
old they are, so a replay never repeats what was done.

| Request | Does |
|---|---|
| `GET /api/admin/events/dead?limit=` | Lists the workspace's dead events: topic, record, attempts, last error, correlation id. Never payloads, which can hold anything. |
| `POST /api/admin/events/{id}/replay` | Queues one again from its first attempt. Consumers that already handled it are not repeated. One whose record has been erased since is refused (409): the erasure took the record out of it, and it would go out without it. |
| `GET /api/admin/webhook-deliveries/dead?limit=` | Lists the webhook deliveries that gave up: webhook, topic, attempts, the endpoint's last answer and error. Never payloads. |
| `POST /api/admin/webhook-deliveries/{id}/replay` | The same for one webhook delivery, refused the same way. |

These are for administrators, stay within their workspace, and are recorded in the audit trail.
`GET /api/admin/operations/{id}` lists everything one operation set off in the workspace (its events, jobs,
process runs, effects and webhook deliveries, how each stands and why, never what it carried; an effect another app's
call made names that app as its `caller`), found by the
id a person quotes: every error carries it as `operation`, and an internal error's message names it.
Administration's **Needs a person** tab finds an operation, and lists dead events, dead webhook deliveries and jobs that failed,
each with why and a button to try it again once its cause is put right. A job tried again runs with the
rights its requester has now, so someone who has left gets nothing from it.

## Topics the platform publishes

| Topic | Payload | Listen to |
|---|---|---|
| `ferrum.user.deprovisioned` | `{user, email, reassign_to, reason}` | Hand over what a package's `handover` declaration cannot express. It also fires when an administrator names a successor later, so handlers must be idempotent. |
| `ferrum.portal.signed_up` | `{user, email, name, role, passed}` | Enrol someone who confirmed a public sign-up to a portal role ([API](api.md#public-sign-up)). |
| `ferrum.companion.signed_out` | `{user, subject, client_id, app, companion, session, reason}` | End a companion program's own session when its person's session ended (`reason`: `signed_out`, `signed_out_everywhere`, `access_ended`, `expired` or `client_removed`). |
| `ferrum.privacy.erased` | `{account, records: [{entity, id, fields}], reason}` | The platform writes the journal from it. No value it erased is in it. |
| `ops.command.{state}` | `{command, state, site, domain, target, operation, workflow_id, outcome}` | A command moved. `workflow_id` says whose it is (`wms-task:{id}`, `mes-run:{id}`). At least once and in any order, so a consumer works from the state, not the sequence. |
| `ops.fact.{kind}` | `{site, kind, at_source, actor, body}` | Something recorded at a site, arriving when the link allowed. `mes.event` is a floor report from a console. |
| `ops.fact.edge.{kind}` | The same, with `kind` one of `attribute`, `event`, `alarm`, `notify`, `request` | What gateways report, and what flows decide. IoT Edge applies each by when it happened: `request` becomes a command if it is still wanted. |
| `quality.hold.released` | `{hold}` | Stock a hold took goes back. The warehouse listens. |
| `mes.run.released` | `{run, materials: [{item, warehouse, qty}]}` | A run needs its material at the station. A managed warehouse opens the moves to its production bin, once per run. |
| `mes.run.closed` | `{run}` | A run finished or was cancelled. What it claimed at a line is free again. |
| `wms.supply.reserved` | `{run}` | A warehouse has the material in hand, and some of it is still on its way. |
| `wms.supply.ready` | `{run}` | All of it is at the line and usable. Only this, or a supervisor saying so, starts the machine. May be said more than once. |
| `wms.supply.short` | `{run, short}` | Some of it cannot be brought, or stopped being usable. The run says so, and a start that has not reached the machine stops. |

## What may happen where

| Where | May | May not |
|---|---|---|
| A document hook, native or plugin | Read and write the database in the document's transaction, emit events, queue mail | Call anything outside. Plugins are refused (`http.fetch`, `sockets.*`). Native hooks are held to it by review. |
| An event handler, a schedule, a route | Call out, in short transactions of its own | Assume it runs once |

A long process that crosses systems is a chain of events with its own state and a way to compensate,
never one transaction held open across a provider or a person.

## Workflows that cross systems

`ferrum_kernel::saga` makes such a chain a durable job: steps that each do one thing outside, in
order, and know how to undo it (`Step::run`, `Step::undo`). Register it as a job handler
(`hooks.add_job_handler(Arc::new(Saga::new("order", "Order", steps)))`) and start it in your
transaction (`Saga::start(tx, tenant, user, "order", input, dedupe)`), so it starts only if that
commits.

* It records what each step gave back as it goes, so a retry, a restart or another replica resumes at
  the step that was under way. A step still runs at least once: `cx.key` is the same every time it
  runs for that workflow, for the other system to recognise a repeat (send it as an
  `Idempotency-Key`). Later steps read earlier ones' answers (`cx.output("reserve")`).
* An error that says it cannot succeed (invalid, forbidden, not found) turns it around at once. Any
  other is tried again with the job's backoff, ten tries forward; after that it turns around too.
* Turned around, it undoes what was done, newest first, each undo at least once, with the tries left.
  It ends `failed` saying why and what was undone. An undo that cannot be done stops it there: the
  error names what a person must still undo, and the job's progress shows what was done and undone.
* Cancelling it (`POST /api/jobs/{id}/cancel`) does not drop it mid-step: it hears the request before
  its next step, undoes what it did, and ends `cancelled`.

A step returning `Error::Wait` comes back later without using a try, as any job does.
