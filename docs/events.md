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
event, automation runs per automation and event. There is no exactly-once, and nothing should
assume it.

**Order** holds within a workspace and a priority, until something fails: a retry goes behind later
events. Anything that depends on order compares `source_version`.

**Fairness.** A batch gives each workspace with work at most 10 events, of 50, and handles workspaces
side by side. So one workspace's backlog, or its slow provider, does not delay another's events.
Within a workspace, mail and push go first and bulk work last. Run an import under
`Trace { priority: Some(PRIORITY_BULK), .. }` to keep it behind what people are waiting for.

**Retries.** A failed event waits 10 s, 20 s, 40 s and so on, up to an hour, and is dead after 15
attempts. One event's handlers get 120 seconds before they count as failed. Webhooks get 12 attempts
of their own, and a request that takes more than 10 seconds fails.

**Dead events** stay in the table for a person to look at; the `ferrum_outbox_dead` and
`ferrum_webhook_dead` metrics count them.

| Request | Does |
|---|---|
| `GET /api/admin/events/dead?limit=` | Lists the workspace's dead events: topic, record, attempts, last error, correlation id. Never payloads, which can hold anything. |
| `POST /api/admin/events/{id}/replay` | Queues one again from its first attempt. Consumers that already handled it are not repeated. |
| `POST /api/admin/webhook-deliveries/{id}/replay` | The same for one webhook delivery. |

These are for administrators, stay within their workspace, and are recorded in the audit trail.

## What may happen where

| Where | May | May not |
|---|---|---|
| A document hook, native or plugin | Read and write the database in the document's transaction, emit events, queue mail | Call anything outside. Plugins are refused (`http.fetch`, `sockets.*`). Native hooks are held to it by review. |
| An event handler, a schedule, a route | Call out, in short transactions of its own | Assume it runs once |

A long process that crosses systems is a chain of events with its own state and a way to compensate,
never one transaction held open across a provider or a person.
