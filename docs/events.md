# Events

What happens after a transaction commits: an event in `outbox`, written in that transaction, handled
by any replica. This is the contract for everything that reacts to a change: webhooks, automations,
plugins, apps' event sinks, mail and push.

## What an event says

| Column | Meaning |
|---|---|
| `id` | The event's identity, increasing with time. Consumers deduplicate on it. |
| `tenant_id` | The workspace. |
| `topic` | `{Entity}.{created,updated,submitted,cancelled,deleted}`, `plugin.{plugin}.{topic}`, or `ferrum.*` for the platform's own. |
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

**At least once.** A worker takes a lease on a batch and handles it outside any long transaction. If
it dies the lease expires and another replica takes the events. Consumers must be idempotent, and
the platform's are: webhook deliveries are unique per webhook and event, plugin runs per plugin and
event, automation runs per automation and event (a scheduled automation's moments per automation, record
and moment). There is no exactly-once, and nothing should
assume it.

**Order** holds within a workspace and a priority, until something fails: a retry goes behind later
events. Anything that depends on order compares `source_version`.

**Fairness.** A batch gives each workspace with work at most 10 events, of 50, and handles workspaces
side by side. So one workspace's backlog, or its slow provider, does not delay another's events.
Within a workspace, mail and push go first and bulk work last: bulk operations, imports, jobs at
bulk priority and sends to a whole list. A mail sent under bulk priority stays there, so 50,000
campaign messages do not stand in front of someone's password reset. Run anything of the kind under
`Trace::at_priority(PRIORITY_BULK)`.

**Retries.** A failed event waits 10 s, 20 s, 40 s and so on, up to an hour, and is dead after 15
attempts. One event's handlers get 120 seconds before they count as failed. Webhooks get 12 attempts
of their own, and a request that takes more than 10 seconds fails.

**Dead events** stay in the table for a person to look at; the `ferrum_outbox_dead` and
`ferrum_webhook_dead` metrics count them.

| Request | Does |
|---|---|
| `GET /api/admin/events/dead?limit=` | Lists the workspace's dead events: topic, record, attempts, last error, correlation id. Never payloads, which can hold anything. |
| `POST /api/admin/events/{id}/replay` | Queues one again from its first attempt. Consumers that already handled it are not repeated. |
| `GET /api/admin/webhook-deliveries/dead?limit=` | Lists the webhook deliveries that gave up: webhook, topic, attempts, the endpoint's last answer and error. Never payloads. |
| `POST /api/admin/webhook-deliveries/{id}/replay` | The same for one webhook delivery. |

These are for administrators, stay within their workspace, and are recorded in the audit trail.
`GET /api/admin/operations/{id}` lists everything one operation set off in the workspace (its events, jobs,
process runs, effects and webhook deliveries, how each stands and why, never what it carried), found by the
id a person quotes: every error carries it as `operation`, and an internal error's message names it.
Administration's **Needs a person** tab finds an operation, and lists dead events, dead webhook deliveries and jobs that failed,
each with why and a button to try it again once its cause is put right. A job tried again runs with the
rights its requester has now, so someone who has left gets nothing from it.

## Topics the platform publishes

| Topic | Payload | Listen to |
|---|---|---|
| `ferrum.user.deprovisioned` | `{user, email, reassign_to, reason}` | Hand over what a package's `handover` declaration cannot express. It also fires when an administrator names a successor later, so handlers must be idempotent. |
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
