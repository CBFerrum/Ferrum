# 21. APIs and their documentation

What an app's own HTTP endpoints look like, how they answer, how callers page, retry and stay safe, how they change
over time, and where an app writes them down. The platform's own API is `docs/api.md`; this guide is about the
endpoints an app adds.

Sources: `crates/kernel/src/hooks.rs` (`Site`, `SiteRequest`, `SiteResponse`), `crates/kernel/src/error.rs`,
`server/src/error.rs`, `docs/api.md` (*Conventions*, *App endpoints*), `docs/plugins.md`, `docs/security.md`
(*Availability*), `docs/plan/apps-ui.md` §4 and §12.12, and two apps: Drive (`src/site.rs`) and Cloud (`src/site.rs`,
`scripts/api_docs.py`, `docs/api.md`, `docs/openapi.json`, `docs/sdk.md`).

## Where an app's routes live

| Kind | Address | Who calls it | Declared by |
|---|---|---|---|
| Signed in | `/api/app/{package}/{path}` | people with a session or an API key, any method | `hooks.add_site("<package>", Arc::new(MySite))` inside the app's registration |
| Public | `/api/site/{tenant}/{package}/{path}` | anyone: public pages, webhooks, devices; rate-limited per address, 300 a minute | the same `Site`, with `req.user == None` |
| A plugin's | `/api/ext/{plugin}{path}`, public `/api/public/{tenant}/{plugin}{path}` | as above, for a WebAssembly plugin | `routes` in the plugin's manifest (`docs/plugins.md`, *Manifest*) |
| Records | `/api/v1/{Entity}` and the rest of `docs/api.md` | everyone, with the caller's rights | nothing: an entity of a package gets them |

Before writing a route, check that the records API does not already do it: lists, filters, create, update, submit,
cancel, actions (`POST /api/v1/{E}/{id}/actions/{action}`), workflow, bulk, import and export come with every entity,
with permissions, audit, events and idempotency built in. A custom action is a better place for a business operation
than a route of your own, because the assistant, MCP, the builder and bulk operations can all call it
([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md)).

## A `Site`

```rust
// ferrum-app-drive/src/site.rs, shortened
pub struct DriveSite;

#[async_trait]
impl Site for DriveSite {
    fn max_body(&self) -> usize { 1 << 20 }                       // JSON routes: 1 MB
    fn max_body_for(&self, method: &str, path: &str) -> usize {    // only the upload routes take more
        let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match (method, p.as_slice()) {
            ("POST", ["upload"] | ["files", _, "versions"]) => max_file_bytes(),
            _ => self.max_body(),
        }
    }
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let path: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        match &req.user {
            None => crate::sharing::public(k, &req, &path).await,   // /api/site/{tenant}/drive/...
            Some(ctx) => app(k, ctx, &req, &path).await,            // /api/app/drive/...
        }
    }
}
```

- `SiteRequest` gives `tenant`, `user: Option<Ctx>` (none on the public prefix), `method`, `path` (relative to the
  package's prefix, starting with `/`), `query`, `headers` (without `authorization` and `cookie`), `body` and
  `client_ip` (for rate limits and audit).
- `SiteResponse::json(status, &value)`, `SiteResponse::text(status, content_type, body)`, and
  `SiteResponse::file(status, content_type, headers, file_id)` for a stored file the server streams (never load a
  large file into memory to answer with it).
- Bodies: 1 MB unless the `Site` says more (`max_body`, `max_body_for` per route), at most 1 GiB
  (`docs/api.md`, *App endpoints*); the server reads no further.
- `x-ferrum-site-api` and `x-ferrum-site-origin` never reach an app from the caller.
- `serves_pages()` is for an app that serves a public website at the root of a `site:{app}` host (Commerce's
  storefront).

The route table is your code. Cloud keeps it as one match in `src/site.rs` with the right each route needs, and
generates its docs from it (below): one list of routes, never two.

## Who may call it

Every route checks rights itself; a route is not protected by being hidden in the interface
(`docs/plan/apps-ui.md` §4 rule 5). Use the caller's `Ctx` for records (the kernel applies roles, scopes and field
groups), and the app's own checks for its own tables. What to answer when the caller may not see something: a 404 that
reads the same as "does not exist", so the address does not say what exists. Cloud's API: "Someone with no standing on
a project gets `404`, so that what exists elsewhere looks the same as what does not exist at all"
(`ferrum-app-cloud/docs/api.md`). The full rule, and when a 403 is right, is in
[05-permissions-and-people.md](05-permissions-and-people.md) and [13-security-rules.md](13-security-rules.md).

Public routes take nothing from a session. Authenticate them by what they carry (a signed link, a webhook signature
checked with the vault, a device token), and count each request against a limit before any lookup.

## Errors with stable codes

Return a kernel `Error`; the server answers `{"error": code, "message": text, "fields"?: [...], "operation"?: id}`
with the status below (`crates/kernel/src/error.rs`, `server/src/error.rs`).

| `Error` | Status | `error` code | Use for |
|---|---|---|---|
| `Validation(vec![FieldError])`, `Error::field(f, msg)` | 422 | `validation_failed` | wrong input, per field; `FieldError::in_row(table, i)` or `.at(path)` for a line |
| `Invalid(msg)`, `Error::invalid(msg)` | 422 | `invalid` | a request that cannot be done as asked |
| `NotFound(what)` | 404 | `not_found` | does not exist, or the caller may not know it does |
| `Conflict(msg)` | 409 | `conflict` | clashes with what exists; a stale `If-Match` |
| `Forbidden(msg)` | 403 | `forbidden` | the caller knows it exists and lacks a right |
| `Unauthorized(msg)` | 401 | `unauthorized` | no or bad credentials |
| `TooManyRequests(msg)`, `Wait(msg, secs)` | 429 | `too_many_requests` | over a limit; `Retry-After` is sent |
| `Retryable(msg)` | 503 | `retry` | a temporary conflict (serialization, deadlock): try again |
| `Unavailable(msg)`, `RetryAfter(msg, secs)` | 503 | `unavailable` | a dependency is down |
| `Db`, `Internal` | 500 | `database_error`, `internal_error` | never shown: logged with an `error_id`, answered "An internal error occurred (operation …)" |
| `Refused { code, status, message, pointer }` | its own | its own | a stable code of the logic platform's (`input_changed`, `stale_approval`, `outcome_unknown`, `recovery_required`, …) or an app's own, such as Cloud's `too_large` (413) in `src/code/reads.rs` |

Rules:

- **The code is the contract; the message is for people.** A client branches on `error` (and the status), never on
  the message's words. The platform's own client does once, `isStale` matching "changed by someone else"
  (Ferrum-Frontend `packages/client/src/errors.ts`): do not copy that.
- A code, once published, keeps its meaning. A new situation gets a new code.
- Field errors name the field as the API names it, so a form can put the message at the field.
- `Retry-After` is sent on 429 and 503; the platform's client honours it (up to 30 seconds) and retries reads and
  writes that carry an idempotency key on 429, 502, 503 and 504, up to four times (Ferrum-Frontend
  `packages/client/src/index.ts`, `send`).
- Never put a secret, a query string, SQL or another workspace's data in a message
  ([20-writing-for-people.md](20-writing-for-people.md)).

## Paging

Never return an unbounded list. The platform's lists take `limit` (at most 500), `offset`, and `cursor` from
`next_cursor` for time-ordered sorts, and answer `{rows, next_cursor?, total?}` (`docs/api.md`, *Documents*). For an
app's own list, use a cursor:

```
GET /api/app/helpdesk/portal/tickets?cursor=…  ->  {"tickets": [...], "next_cursor": "…" | null}
```

- The cursor is opaque to the caller; base it on a stable order (a time and an id), so rows added meanwhile neither
  repeat nor go missing.
- Cap the page size on the server whatever the caller asks.
- `total` only when it is cheap or asked for (`count=true` on the platform's lists).
- A list for a public page has a hard cap and a statement timeout (published lists: 1,000 rows, 5 seconds;
  `docs/security.md`, *Published lists*).

## Idempotency: safe to send again

Anything that creates something or has an effect outside takes an `Idempotency-Key`, so a request whose answer was
lost can be sent again and is done once.

- The platform: `POST /api/v1/{entity}`, `PATCH /api/v1/{entity}/{id}` and `POST /api/v1/{entity}/{id}/actions/{action}`
  accept `Idempotency-Key` (8 to 200 characters); a replay returns the original answer with `200` and
  `Idempotent-Replayed: true` (`docs/api.md`, *Conventions*).
- Cloud requires one on every create and refuses without it: "this request needs an Idempotency-Key header, so that
  repeating it is safe"; a key used for a different request is a conflict (`src/site.rs`, `src/operations.rs`).
- Store the key with a digest of the request in the same transaction as the effect; answer a repeat with the first
  answer; refuse the same key with a different request. Drive's uploads keep the key with a digest of the request and
  refuse "this Idempotency-Key was used for another upload" (`src/site.rs`).
- The browser client sends one when given `idempotencyKey` (`newKey()` makes one), and only then retries a write.
  For work kept offline and sent later, `idempotencyDigest` binds the key to what the request does rather than its
  body (Ferrum-Frontend `packages/client/src/index.ts`, `RequestOptions`).
- There is no kernel helper for an app's own routes: each app keeps its keys. Use the same rules.

Optimistic concurrency: records carry `version`; send `If-Match: "v<version>"` on update, delete, submit and cancel,
and a mismatch is `409`. An app's own record-like resources should do the same.

## Long work

A request never waits on a slow outside system or a person. Start the work (a job, a process, a saga), answer `202`
with where to look, and let the caller ask (`GET /operations/{id}` in Cloud) or hear an event. An outcome nobody knows
yet is an answer of its own (`outcome_unknown`, Cloud's `unresolved`), never retried blindly
([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md); `docs/events.md`, *What may happen where*).

## Rate limits

- Each person has a request budget (3,000 a minute across sessions and keys by default) and each client address one
  (6,000 a minute), shared across replicas (`docs/security.md`, *Availability*). Public routes: 300 a minute per
  address.
- For your own limits (sign-ups, codes sent, messages), use `Kernel::rate_limit(key, window, max, message)`: counted in
  the database, so it holds across replicas (`crates/kernel/src/lib.rs`; `docs/limits.md`). Key it by what you are
  protecting (the address, the workspace, the person), and say in `message` when to try again.
- A limit counted in memory on one replica is not a limit when there are several.

## Versions and change

- The platform's API carries its major version in the path (`/api/v1`); within a major, only additions
  (`docs/plan/apps-ui.md` §4 rule 4). Hold your own routes to the same: add fields and routes, never change what one
  means, never remove one in a minor release.
- Readers ignore what they do not know; writers do not send what was not asked for. Cloud's events carry a `schema`
  that a new field does not change (`ferrum-app-cloud/docs/sdk.md`).
- Every call from an interface carries `X-Ferrum-UI: {app}@{version}` (`packages/client/src/index.ts`). The plan
  (`docs/plan/apps-ui.md` §12.12) adds that a call that cannot be served for an old interface answers `409 ui_outdated`,
  that an app's endpoints accept the previous minor of its own interface, and that an app's API answers
  `503 app_upgrading` while an upgrade migrates a workspace. Neither answer is in the code today, and nothing checks
  that a page open across an upgrade keeps working: test it by hand when you change a route the interface calls.
- Deprecating: the plan's rule is two platform minors with warnings before a removal (`docs/plan/apps-ui.md` §4
  rule 4). For an app: say it in the CHANGELOG with the version that will remove it, keep it working until then, and
  remove it in a release whose version says so ([16-releasing.md](16-releasing.md)).

## Documenting your API

Where:

- `docs/api.md` of the platform says each app documents its endpoints in its repository
  (`packages/<package>/README.md`; Drive's `packages/drive/README.md` holds its access model). Put the route reference
  in `docs/` of the app's repository (`docs/api.md`) and link it from the README.
- The platform's live OpenAPI document (`GET /api/openapi.json`, per workspace, with every customization) describes
  records, not an app's own routes (`server/src/openapi.rs` does not read the sites).

How, from Cloud, the example to copy:

- `scripts/api_docs.py` reads the route table in `src/site.rs` and writes `docs/api.md` (method, path, who may call
  it), `docs/openapi.json` and a TypeScript client `sdk/typescript/client.ts`; `scripts/api_docs.py --check` fails
  when any of them is out of date, so the docs cannot drift from the code.
- `docs/sdk.md` says what every client must do: send an idempotency key and the same one on every try; retry what was
  lost and 429 and 503, never a refusal; stop on `unresolved`, which is an answer.
- Bodies are not in the generated table because the route table does not know them; `docs/sdk.md` says where each
  one's fields are. Say so in yours if it is the same.

What every app's API docs say, at least:

- [ ] each route: method, path, who may call it (the role or right), what it returns, its errors and codes;
- [ ] which routes take an `Idempotency-Key`, and what a repeat answers;
- [ ] paging: the cursor, the cap;
- [ ] limits: body sizes, rate limits, timeouts;
- [ ] events the app publishes and their payloads, if other apps may listen
  ([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md));
- [ ] public routes: how they authenticate and what they never reveal;
- [ ] what is not built or never run against a real counterpart (a webhook sender, a device).

## Checked by

- The kernel and server tests of the error mapping (`crates/kernel/src/error.rs` tests, `server/src/error.rs`).
- An app's own: Cloud's `scripts/api_docs.py --check` keeps its docs, OpenAPI and SDK in step with its routes. No
  platform script checks an app's API docs, paging, idempotency or versioning.
- Reviewed, not checked: everything else in this guide.

## See also

- [05-permissions-and-people.md](05-permissions-and-people.md), [06-events-jobs-and-logic.md](06-events-jobs-and-logic.md),
  [11-integrations.md](11-integrations.md) (webhooks in, calls out), [13-security-rules.md](13-security-rules.md),
  [20-writing-for-people.md](20-writing-for-people.md)
- `docs/api.md`, `docs/events.md`, `docs/plugins.md`, `docs/limits.md`
