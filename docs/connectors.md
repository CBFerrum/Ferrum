# Connectors

An app owns a workflow and its records. A connector translates a provider's model, credentials,
events and limits into them, so that a provider's change does not mean rewriting the app. A
customization changes behaviour inside an app; neither is the other.

A provider implements `Connector` (crates/kernel/src/connect.rs), compiled in and registered with
`HookRegistry::add_connector`, or as a plugin (below). It describes what its API really offers
(`ConnectorInfo`: streams, what it writes, webhooks, API version, what using it takes, and each thing
this kind of account can do as a named capability), rather than a common denominator. Everything else
is shared:

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
| Being a good client | A provider that says to wait (`Error::Wait`, or `TooManyRequests`) is left alone for that long. One that fails three times in a row is left alone for 30 s, doubling up to an hour. Every job of the connection waits together, and so does every connection on the same provider account (the same provider and credentials, known by a keyed fingerprint that reveals nothing of them), in any workspace: a provider's limit is the account's. Waiting costs a job no attempt; a call that failed does, so what can never succeed still ends as a dead letter. |
| Operations | `GET /api/admin/connector-instances`: each connection's `health`, told apart: `authentication` (`none`, `untested`, `ok`, `failing`), `freshness`, `backlog`, `dead_letters`, `drift` and `quota`. An upstream outage does not read as a healthy connection, and a green badge is not on offer. |

A provider's call never holds a database transaction open.

## A connector as a plugin

A provider's adapter can be a sandboxed plugin instead of code in the server, so it is versioned and
signed apart from the platform. Declare it in the manifest:

```json
{
  "http_hosts": ["api.example.com"],
  "credentials": [{"name": "token", "hosts": ["api.example.com"]}, {"name": "webhook_secret"}],
  "connector": {
    "label": "Example", "family": "crm", "api_version": "2026-01",
    "streams": ["people"], "writes": ["Contact"], "webhooks": true,
    "capabilities": ["read_people", "write_people"]
  }
}
```

Each operation is one call with `{"kind": "connector", "op", "args", "connection": {"id", "config"}}`:

| `op` | `args` | Answer |
|---|---|---|
| `check` | | `{"account"}`: proves the credentials, changing nothing |
| `discover` | | `{"streams": [{"stream", "fields"}]}`: what this account holds, customizations included |
| `pull` | `{stream, cursor}` | `{"records": [{"id", "version"?, "deleted"?, "fields"}], "next"}` |
| `push` | `{key, entity, record, external_id, fields}` | `{"id"}`. `key` is the same for every attempt at a change |
| `webhook` | `{headers, body}` | `{"hooks": [{"event_id", "stream", "record"}]}`. Refuse what is not signed |

Any operation may answer `{"wait": seconds, "why"}` (the provider's rate limit), `{"unavailable":
"why"}` (it is having trouble) or `{"error": "why"}` (it refused).

The module serves every account. A connection's credentials are the plugin's credentials for that
call, held by the host: it names them (`fetch_with`, `verify_hmac_sha256`) and never reads them, and
each goes only to the hosts its manifest declared. `plugins/examples/connector-demo` is the whole
shape, and `crates/plugins/tests/connector_it.rs` drives it through the real machinery.

Operation names are stable: a new operation is added beside them, and an older module that does not
know one answers with an error rather than misbehaving.

## What finished means

A connector is finished only with a working configuration, an authenticated test connection, evidence
against the real provider or its approved sandbox, synchronization and reconciliation tests, failure
recovery, and a stated capability description. The tests here run against a stand-in: they prove this
machinery, and nothing about any provider. Where a provider's programme needs approval or certification,
writing an adapter does not establish it.

## API

Administrators only: `GET /api/admin/connectors` (what this server can connect to); `GET`/`POST
/api/admin/connector-instances`; `PUT`/`DELETE /api/admin/connector-instances/{id}`; `POST .../{id}/test`, `GET .../{id}/schema`, `POST .../{id}/sync`
`{from_start}`, `.../{id}/reconcile`; `GET .../{id}/drift`; `GET .../{id}/activity` (its latest reads, sends and comparisons with how each went, and the records each stream links); `POST /api/admin/connection-drift/{id}/resolve`. A failed run is tried again with `POST /api/jobs/{id}/retry`.
