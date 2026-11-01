# Connectors

An app owns a workflow and its records. A connector translates a provider's model, credentials,
events and limits into them, so that a provider's change does not mean rewriting the app. A
customization changes behaviour inside an app; neither is the other.

A provider implements `Connector` (crates/kernel/src/connect.rs) and is registered with
`HookRegistry::add_connector`. It describes what its API really offers (`ConnectorInfo`: streams,
what it writes, webhooks, API version, and what using it takes), rather than a common denominator.
Everything else is shared:

| Concern | What happens |
|---|---|
| Credentials | Sealed apart from the record, bound to the connection, moved by key rotation, never read back. A connection stays off until `test` proves them. `DELETE` forgets them. |
| Mapping | Per stream: which entity, which fields, how enum values translate. Only mapped fields come in. |
| Synchronization | A job per run, a page at a time, storing its cursor after each. A worker lost part-way, or a provider saying slow down, resumes from the cursor. Each record is one transaction. A record the workspace refuses is recorded for a person, and the rest carry on. |
| Conflicts | When both sides changed: `source` (the provider wins), `local` (ours stays), or `review` (ours stays, and a person is told). |
| Tombstones | A record gone at the provider: `keep` it, or `delete` it. Its link goes either way. |
| Delivery | A local change is a job whose key is the same for every attempt, up to 8 tries with backoff. Then it is a dead letter, retried with `POST /api/jobs/{id}/retry`. What came from the provider does not go back to it. |
| Webhooks | `POST /api/connect/{connection}/webhook`. The connector authenticates what arrives, repeats of an event are ignored, and the work happens in a job, so the provider gets its answer at once. |
| Reconciliation | Compares what the provider has with what we hold, and records `differs`, `missing_here` and `missing_there` for a person. It repairs nothing by guesswork. |
| Operations | `GET /api/admin/connections`: last success, last error, backlog, dead letters, drift. |

A provider's call never holds a database transaction open.

## What finished means

A connector is finished only with a working configuration, an authenticated test connection, evidence
against the real provider or its approved sandbox, synchronization and reconciliation tests, failure
recovery, and a stated capability description. The tests here run against a stand-in: they prove this
machinery, and nothing about any provider. Where a provider's programme needs approval or certification,
writing an adapter does not establish it.

## API

Administrators only: `GET /api/admin/connectors` (what this server can connect to); `GET`/`POST
/api/admin/connections`; `PUT`/`DELETE /api/admin/connections/{id}`; `POST .../{id}/test`, `.../{id}/sync`
`{from_start}`, `.../{id}/reconcile`; `GET .../{id}/drift`; `POST /api/admin/connection-drift/{id}/resolve`.
