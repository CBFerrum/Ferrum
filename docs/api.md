# Ferrum HTTP API

Base URL: the server root (dev `http://127.0.0.1:5150`). JSON everywhere. A live, tenant-specific
OpenAPI 3.1 document is at `GET /api/openapi.json` (it includes every customization).

## Conventions

| Topic | Rule |
|---|---|
| Auth | `Authorization: Bearer <session token>` from `/api/auth/login`, or `Bearer fk_<prefix>_<secret>` (API key) |
| Money & decimals | **Strings** (`"1234.5"`), never JSON floats. Send strings too; numbers are accepted but avoid floats |
| Dates | `YYYY-MM-DD`; date-times RFC 3339 UTC (`2026-10-28T10:15:00.000Z`). Business dates ("today", posting dates and times, defaults) are local to the workspace's time zone (`GET /api/auth/me` → `timezone`, `today`) |
| Links | UUID strings. Add `titles=true` to list/get to receive `_titles: {field: "Display title"}` on records and child rows. As in ERPNext, a title shows even when the caller cannot open the linked record (only its title field or number, nothing else) |
| Concurrency | Records carry `version`; responses send `ETag: "v<version>"`. Send `If-Match: "v<version>"` on update/delete/submit/cancel; a mismatch → `409` |
| Idempotency | `POST /api/v1/{entity}` accepts `Idempotency-Key` (8–200 chars). A replay returns the original record with `200` and `Idempotent-Replayed: true` |
| Errors | `{"error": code, "message": text, "fields"?: [{"field", "message", "row"?: [tableField, rowIndex]}], "error_id"?}`; codes: `validation_failed` (422), `invalid` (422), `not_found` (404), `conflict` (409), `forbidden` (403), `unauthorized` (401), `too_many_requests` (429), `retry`/`unavailable` (503, with `Retry-After`) |
| Document status | `docstatus`: 0 draft, 1 submitted, 2 cancelled. Submitted documents are immutable; cancel + amend to change |

## Auth — `/api/auth`

| Method & path | Body → response |
|---|---|
| `POST /login` | `{tenant, email, password}` → `{token, token_type: "Bearer", expires_in}` (rate limited; 5 failures lock the user 15 min). With two-factor sign-in on → `{second_factor: true, ticket, expires_in: 300}`; in a workspace that requires it, for someone who has yet to set it up → `{two_factor_setup: true, token, expires_in: 900}`, a token good only for `/me` and `/2fa` (setup) |
| `GET /sso?tenant=` | → `{providers: [{id, label}]}`: the workspace's single sign-on buttons (none for an unknown workspace) |
| `POST /sso/start` | `{tenant, provider, redirect_uri, binding}` → `{url}` to send the browser to. `redirect_uri` is the sign-in page (registered with the provider, which sends the browser nowhere else); `binding` is a random secret (16–200 characters) the browser keeps to finish |
| `POST /sso/finish` | `{state, code, binding}` (what the provider sent the browser back with) → `{token, …}`. Each sign-in works once, within ten minutes, in the browser that started it. The provider's own two-factor rules apply instead of Ferrum's |
| `POST /login/second-factor` | `{ticket, code}` (six digits from the authenticator app, or a recovery code) → `{token, …}`. Each app code is accepted once; wrong codes count toward the same lockout as wrong passwords |
| `GET /me` | → `{tenant, user: {id, email, full_name, is_admin, roles}, default_company, scopes, fiscal_year_start_month, timezone, today, packages, two_factor_setup}` |
| `GET /2fa` | → `{enabled, pending, recovery_codes_left, required}` |
| `POST /2fa/begin` | → `{secret, uri (otpauth://), qr_svg}`: a new secret for an authenticator app (TOTP, SHA-1, 6 digits, 30 s); it takes effect once confirmed |
| `POST /2fa/enable` | `{code}` → `{recovery_codes}` (ten, shown once; and `token`, a full session, when called with a setup-only token) |
| `POST /2fa/recovery-codes` | `{code}` → `{recovery_codes}`: new ones replace the old |
| `POST /2fa/disable` | `{password}` or `{code}` → `204` |
| `POST /logout_all` | revokes every session of the caller (and clears the session cookie) |
| `POST /logout` | signs this browser out: its session ends, with the tokens bound to it, and the cookie is cleared → `204` |
| `POST /token` | (session cookie) → `{token, token_type, expires_in, tenant, email}`: a bearer token for a page that still sends one, bound to the browser's session |
| `GET /sessions?user=` | → `{sessions: [{id, user_id, actor, created_at, last_seen_at, expires_at, user_agent, ip, current}]}`: the signed-in browsers of the caller (administrators: of `user`) |
| `DELETE /sessions/{id}` | signs one browser out (one's own; administrators: anyone's) |
| `POST /step-up` | `{password, code?}` → `204`: the person proved themselves again; sensitive actions accept the session for ten minutes |
| `POST /password` | `{current_password, new_password}` (policy: ≥10 chars, 3 of lower/upper/digit/symbol) |
| `GET /api_keys`, `POST /api_keys` `{name, expires_at?, scope?}` → `{id, key}`, `DELETE /api_keys/{id}` | API keys (secret shown once). `scope: "dav"` makes an app password for calendar and contacts apps: it signs them in at `/api/dav/…` over HTTP Basic (email and password) and is refused as a bearer token |

**Browser sessions** ([plan](plan/apps-ui.md) §12.3). When a page of the site signs in
(`Sec-Fetch-Site: same-origin`), the answer also sets the session cookie: `__Host-ferrum` where the
server is reached over https, `ferrum_session` on a plain-http development server; `HttpOnly`,
`SameSite=Lax`, `Path=/`, for `FERRUM_SESSION_DAYS` (14). Each browser is a session of its own,
listed and signed out one by one. `session: "cookie"` in the body of `/login`,
`/login/second-factor` or `/sso/finish` asks for the cookie alone (the page gets no token);
otherwise the token returned is bound to the browser's session. API clients (no Fetch Metadata) get
tokens only, as before. With the cookie:
* requests that change something must come from the site's own pages: `Sec-Fetch-Site:
  same-origin`, or, without that header, an `Origin` equal to the host; `same-site` and
  `cross-site` are refused (403);
* the cookie's token is refused as a bearer token, and a bearer token in place of the cookie;
* sockets (`/ws/live`, `/ws/docs/…`) accept the cookie at the upgrade when `Origin` is the host;
* on a host registered for a workspace, a cookie of another workspace is refused.

Document actions may declare `params` (`[{name, label, type: data|text|select|int|date, options?,
required?, description?}]`): the form asks for them, prefilled from the record's fields of the same
name, and sends them as the action's body. An action whose result carries `file: {name, type,
content}` makes the form save that file (e-invoice JSON, say).

## Metadata — `/api/meta`

* `GET /api/meta` → `{version, packages, modules: [{name, label, icon, order, items: ["EntityName" | "report:<name>"]}], entities: [{name, label, label_plural, kind, module, icon, submittable, custom, permissions}], warnings}`
* `GET /api/meta/{Entity}` → full description used to render forms and lists:
  `{name, label, label_plural, kind (master|document|child|settings|ledger), module, title_field, submittable, company_scoped, numbering, list: {columns, sort}, form: [{label, fields, columns (1-4), collapsible, tab?, visible_if?}], search_fields, rules, actions: [{name, label, docstatus[], permission, confirm?, condition?, read_only?, params?, group?}], workflow?: {initial_state, states: [{name, docstatus, edit_roles}], transitions: [{action, from, to, roles, condition?, not_creator}]}, fields: [Field], children: {tableField: {entity, label, fields: [Field]}}, permissions: {read|write|create|delete|submit|cancel|export|import: "none"|"own"|"all"}}`
* `Field`: `{name, label, type, required?, unique?, read_only?, hidden?, in_list?, in_filter?, options?, to?, of?, precision?, currency?, min?, max?, max_length?, default?, default_expr?, computed?, visible_if?, required_if?, read_only_if?, fetch_from?, description?, link_filters?, storage: "column"|"ext", can_read, can_write}`.
  Types: `data text markdown code int decimal currency percent date datetime check select link table json email phone url attach color secret`.
  `visible_if`/`required_if`/`read_only_if` are CEL over `doc` (and `parent` in child rows); the server is the authority.
  CEL also sees `user` and `today`, `now`, `time` (local to the workspace).
* `GET /api/meta/contributions/{point}` → `{point, items}`: what enabled apps contribute
  ([plugins](plugins.md#extension-points-and-add-ons)). Each item is the contribution plus its
  `app` and `version`. A point is either:
  * a screen slot: `ui.page`; `ui.widget`; `ui.panel`, listing only panels on entities the caller
    can read; `ui.settings`, for administrators only;
  * an enabled app's extension point; 404 when no enabled app offers it.
* Form sections: sections sharing a `tab` label show under that tab (untabbed sections form the first
  tab); a section's `visible_if` (CEL over `doc`) hides it while false.
* `secret` fields are write-only: records carry `true` when a value is set (never the value). Send
  text to set, `null` or `""` to clear, `true` to leave unchanged. Values are sealed with AES-256-GCM
  and never reach events, the audit log, exports or webhooks.

## Documents — `/api/v1/{Entity}`

| Method & path | Notes |
|---|---|
| `GET /api/v1/{E}` | Query: `filters` (JSON: `[["field","op",value],...]` or `{"field": value}`; ops `= != > >= < <= in "not in" like "not like" between is`; paths `child_table.field`, `link_field.target_field`; `["_tag","=","urgent"]` filters by tag), `fields` (comma list), `sort` (`field asc|desc`), `limit` (≤500), `offset`, `cursor` (from `next_cursor`, for `created_at`/`updated_at` sorts), `search`, `titles`, `count` → `{rows, next_cursor?, total?}`. Filters, sorts and search only use fields the caller can read (a `403` otherwise; a default sort the caller cannot read falls back to `created_at desc`), and a `link.field` filter only matches linked records the caller may read |
| `POST /api/v1/{E}` | create (draft for submittable entities) → `201` record. `?submit=true` creates and submits in one transaction (with `Idempotency-Key`, a replay never submits twice) |
| `GET /api/v1/{E}/{id}` | `titles` defaults to true |
| `PATCH /api/v1/{E}/{id}` | partial update; child tables are replaced by the given rows (rows with a known `id` are patched, rows without `id` are added, missing rows are removed) |
| `DELETE /api/v1/{E}/{id}` | drafts/cancelled/masters → `204` |
| `POST /api/v1/{E}/{id}/submit` · `/cancel` · `/amend` | lifecycle; amend returns the new draft |
| `POST /api/v1/{E}/{id}/actions/{action}` | custom actions from metadata (e.g. `make_delivery_note`); body = params; returns action JSON (often `{entity, id}` of a created record). A `read_only` action runs without locking or auditing, in a read-only transaction |
| `POST /api/v1/{E}/{id}/workflow/{action}` | workflow transition |
| `POST /api/v1/{E}/_calc` | preview: runs defaults, fetches and calculation hooks on an unsaved body (include `id` to start from a saved record) → computed document, nothing written. Use for live totals |
| `GET /api/v1/{E}/{id}/print` | HTML print format; `?format=pdf` → the same as a PDF (`application/pdf`; `503` when the server has no PDF renderer, see `GET /api/version` → `pdf`) |
| `POST /api/v1/{E}/{id}/email` | `{to, subject?, message?, pdf?: true}` → `202 {queued, to, subject, attachments}`: emails the document through the server's mailer, its print format attached as a PDF (stored on the record), and notes it on the record's timeline. Needs export permission on the entity; 30 per user per minute. `503` without a mailer, or for a PDF when the server has no PDF renderer or its mailer cannot attach (`GET /api/version` → `mail`, `mail_attachments`, `pdf`) |
| `GET /api/v1/{E}/{id}/audit` | `{events: [{at, actor, actor_name, action, version, diff}], chain_valid}` |
| `GET /api/v1/{E}/_export` | CSV (export permission), or Excel with `format=xlsx` (field labels as headers, numbers as numbers); same filters as list, at most 100,000 rows |
| `POST /api/v1/{E}/_import` | JSON array (≤5000) → `{created, ids, errors: [{row, error, fields?}]}` (`207` if some failed) |
| `POST /api/v1/{E}/_bulk` | one operation on a selection: `{ids: [...]}` or `{filters: [...]}` (≤500 records) plus `{"op": "update", "values": {...}}`, `{"op": "delete"}`, `{"op": "submit"}`, `{"op": "cancel"}`, `{"op": "workflow", "action"}`, `{"op": "action", "action", "params"?}`, `{"op": "assign", "user", "note"?, "due_date"?, "priority"?}`, `{"op": "add_tags" \| "remove_tags", "tags"}` → `{succeeded, failed, results: [{id, ok, error?}]}`. Each record runs in its own transaction with the caller's permissions; `error` has the API error shape |

## Collaboration (any record the caller can read)

| Method & path | Notes |
|---|---|
| `GET /api/v1/{E}/{id}/comments` | → `{comments: [{id, body, created_by, author, author_email, created_at, edited_at, mine}]}` |
| `POST /api/v1/{E}/{id}/comments` | `{body, mentions?: [user id]}` → `201 {id}`; mentioned users and followers are notified |
| `PATCH /api/comments/{id}`, `DELETE /api/comments/{id}` | `{body}`; authors (and administrators for delete) only |
| `GET /api/v1/{E}/{id}/assignments`, `POST …` | `{user, note?, due_date?, priority?}` → `201 {id}`; the assignee is notified |
| `GET /api/assignments?status=Open\|Done\|all` | the caller's to-dos across records (open by default) |
| `POST /api/assignments/{id}/close` | `{status: "Done" \| "Cancelled"}` |
| `GET /api/v1/{E}/{id}/tags`, `PUT …` `{tags: [...]}` | tags; `GET /api/tags/{E}` → `{tags: [{tag, count}]}` over the records the caller can read |
| `GET /api/v1/{E}/{id}/follow`, `PUT …` `{follow: bool}` | followers get a notification when the record changes |
| `GET /api/v1/{E}/{id}/attachments` | → `{attachments: [{id, file_name, content_type, size_bytes, created_by, created_at, url}]}`; `DELETE /api/files/{id}` removes one (an attachment only by someone who can edit its record; a loose upload by its uploader) |
| `GET /api/users` | directory of active users `[{id, full_name, email}]` for mentions and assignment |

## Preferences — `/api/prefs` (the signed-in user's own)

`GET /api/prefs` → `{prefs: {scope: {value, version}}}`; `PUT /api/prefs/{scope}` `{value, version?}` →
`{scope, value, version}` (a scope is 1-120 of `A-Z a-z 0-9 : _ . -`, e.g. `list:SalesInvoice`; values
≤64 KB; sending the `version` you read makes a newer write elsewhere a `409`); `DELETE /api/prefs/{scope}`
resets it. At most 500 scopes per user.

## Live updates — `GET /ws/live` (WebSocket)

Send `{"auth": "<token>"}` first (tokens never go in URLs) → `{"ready": true, "user"}`. Then
`{"subscribe": ["SalesOrder", …]}` / `{"unsubscribe": […]}` → `{"subscribed": […], "denied": […]}`.
Events: `{"event": "SalesOrder.updated", "entity", "id", "actor"}` for committed changes to records the
user may read, `{"notification": {id, title, entity, record_id}}` for the user's notifications and
`{"push": {"kind", "data"}}` from apps (chat). Works on every replica (PostgreSQL LISTEN/NOTIFY).

## Live documents — `GET /ws/docs/{package}/{id}` (WebSocket)

A document several people edit at once: today Drive's documents (`.fdoc` files; package `drive`,
id the file's). Sign in as on `/ws/live` (`{"auth": "<token>"}` first, or an `Authorization` header)
→ `{"ready": true, "access": "write" | "read", "user": {"id", "name", "color"}}`. Closes before that:
`4401` (sign in again), `4404` (no such document, or no access to it), `1011` (try again).

Then binary frames carry the Yjs sync protocol (y-protocols 1.0: sync steps 1 and 2, updates,
awareness, awareness queries) for the XML fragment `default`, as TipTap's Collaboration extension
writes it. The server also sends text:
* `{"stored": n}`: this connection's first `n` change messages (its sync step 2 and its updates) are
  stored. Until then they are unsaved; after a reconnect, the sync step resends what the server lacks.
* `{"access": "read" | "write"}` when the person's access changes (checked every minute). Access
  taken away closes the socket with `4403`.
* `{"error": "…"}` when a change is refused: anything new from a viewer, or growth past 32 MB.
  Refused changes are never acknowledged. A viewer's sync step that adds nothing is fine.

Cursor names and colours are the server's, whatever a client says; a cursor state is at most 4 KB.
Limits: 8 MB per message and 1,200 messages per 10 seconds (closed with `1008`). A connection that
falls 1,024 messages behind is closed with `1013` and resyncs. Other closes: `4400` for a message
that is not the protocol. Exports: Drive's `GET /api/app/drive/files/{id}/content?format=html|pdf|txt`.

## App endpoints

Apps add HTTP endpoints under `/api/app/{package}/{path}` (signed in, any method) and
`/api/site/{tenant}/{package}/{path}` (public pages, webhooks; rate-limited per IP, 300/min). Each
app documents its own in `packages/<package>/README.md` of its repository. Request bodies are
limited per app (1 MB unless the app allows more, at most 32 MB).

## Screens of apps

* `GET /api/ext-ui/{tenant}/{app}/{version}/{path}` → a file under the `ui/` of an enabled app, at
  its installed version ([plugins](plugins.md#screens)).
  * No sign-in is needed: sandboxed frames send none.
  * Headers: `Content-Security-Policy: sandbox allow-scripts allow-downloads; default-src 'none';
    … frame-ancestors 'self'`, `ETag` (the bundle digest) and `Cache-Control: no-cache`.
  * HTML pages answer 403 unless they are loaded in a frame (`Sec-Fetch-Dest: iframe`).
* `X-Ferrum-App: <app>` on a signed-in request marks a call from that app's screen.
  * It may go only to `/api/v1/…`, `/api/meta/…` and `/api/ext/<app>/…`; anything else is 403.
  * The caller's rights are narrowed to the app's declared permissions and its own entities, without
    administrator power.

## The shell ([plan](plan/apps-ui.md))

Pages, for browsers, with the session cookie:

* `GET /{route}/…` → an app's page: its `index.html` with the bootstrap, the theme, the shell
  runtime and `<ferrum-bar>` put in, under a Content-Security-Policy of its release. Signed out →
  `303` to `/login?next=…`. Otherwise a page saying what is wrong: nothing installed there (404), no
  access (403), no interface for this version (503), the workspace suspended (403). `/{route}`
  redirects to `/{route}/`; an alias, to the route.
* `GET /{route}/sw.js` → the app's own service worker, or one that removes itself where nothing the
  person may use lives any more.
* `GET /open/{entity}/{id}` → `303` to the screen of an app that shows the record and that the
  person may use (`?via={app}` first, then `priority`, then install order), else the shell's
  records page (today's, for a workspace not on the shell) if the person may read the entity, else
  403 (404 for an entity the workspace does not have). `GET /open/{entity}` → the same for the list (an app's `shell.open` `list`). The rest of the
  query goes along.
* `GET /{public path}/{workspace}/…` (an app's `shell.public`, such as `/drive/s/acme/{link}` and
  `/meet/guest/acme/{link}`) → the app's page for people outside the workspace: no session, no bar,
  `Referrer-Policy: no-referrer`, `X-Robots-Tag: noindex, nofollow`; 404 when the workspace, the app
  or its page is not there (or the host serves another workspace). Rate-limited per address.
* `GET /login?next=` → the sign-in page.
* `GET /_ui/{app}/{digest}/{path}` → a file of an interface release: public, `immutable`.
* `GET /_shell/v1/{file}` → the shell's own files (`runtime.js`, `live-worker.js`, …), the newest of
  v1; `GET /_shell/theme/{hash}.css` → the theme.

For the bar, with the cookie or a token:

* `GET /api/shell` → `{v, shell, workspace, person, apps: [{id, app, title, description, icon,
  color, compact, url, family}], create: [{app, label, entity, url}], intents: [{app, action, type,
  title, url}], sidepanels: [{id, app, title, icon, contextual, url}], families: [{id, title, apps}],
  legacy}`: what the caller may use. `family` names the head of the entry's family (`families` lists
  them, heads first). Entries into today's web app (`classic: true`) appear only for apps whose
  installed version has no interface of its own. `ETag`.
* `GET /api/shell/version?app=` → `{app, version, release}`: the interface release the workspace
  runs, so an open tab can tell it is outdated.

## Reports — `/api/reports`

* `GET /api/reports` → `{reports: [{name, label, module, params: [{name, label, type: "link"|"date"|"select"|"check", to?, options?, required?, default?}]}]}`
* `GET /api/reports/{name}?param=value...` → `{name, label, columns: [{name, label, type}], rows: [{..., _indent?, _bold?}], totals?: {...}, meta?: {...}}`
* The platform's own reports sum up all records of their entity, so they need access to all of
  them: someone limited to their own records, or by a scope other than companies (reports run per
  company and check that scope), gets `403` and does not see them in the list. Package reports
  (and Studio queries) apply the caller's row scope to their rows instead. A package report source
  that only narrows to the caller's own records declares `"scoped": "owner"` in its catalog entry,
  and one that sums up everything `"scoped": false`; those get the same check.
* Studio's saved reports run at `GET|POST /api/app/studio/reports/{id}/run` (runtime `filters`,
  `search`, `from`, `to`, `sort`, `limit`) and download at `GET /api/app/studio/reports/{id}/export`
  with `format=csv` (default) or `format=xlsx`: every row up to 10,000 with the same runtime
  settings, titles instead of ids and a totals line; the workbook keeps numbers as numbers and
  dates as dates in a sheet named after the report. Exports need export permission on the
  report's entity.

## Administration (administrators only, except notifications)

Workspace settings: `GET /api/admin/settings` → `{timezone, fiscal_year_start_month, require_two_factor}`;
`PUT` with any of them (`timezone` is an IANA name such as `Asia/Kolkata`; `require_two_factor`
applies from each person's next sign-in; `require_sso` refuses password sign-in to everyone but
administrators and needs an enabled single sign-on provider).

Single sign-on: `/api/admin/sso` (GET → `{providers: [{id, label, issuer, client_id, has_secret,
domains, auto_create, default_roles, enabled, linked_users}]}`; POST `{label, issuer, client_id,
client_secret?, domains[], auto_create, default_roles[], enabled}` → `{id}`, the issuer's discovery
document fetched to check it), `/api/admin/sso/{id}` (PUT the same — `client_secret` left out keeps
the stored one, `""` clears it; DELETE unlinks the people linked through it).

`/api/admin/users` (GET, POST `{email, full_name, password?, roles[], is_admin}`), `/api/admin/users/{id}/roles` (PUT `{roles}`),
`/scopes` (PUT `{entity, ids[], default?}`), `/active` (PUT `{active}`), `/password` (PUT `{password}`), `/two-factor/reset` (POST: turns the person's two-factor sign-in off, lifts a lockout and signs them out everywhere — for a lost phone), `/sso/unlink` (POST: forgets the identity at a single sign-on provider the account is linked to); the user list shows `two_factor`, `sso` and `locked`;
`/api/admin/personal-data?email=` (GET: every record the installed packages declare as the person's — `personal_data` in package manifests, `{entity, match: <email field>, erase: [fields], where?: {field: value}}` — with its data, and their login account) and `/api/admin/personal-data/erase` (POST `{email, reason}`: blanks the declared fields, a placeholder where a value is required, and empties declared child tables and JSON fields; redacts those fields in the records' audit history, each event keeping its hash and recording the redaction; anonymizes and closes the login account; refused for administrators and oneself) → `{erased: [{entity, id}], account_erased, audit_events_redacted}`;
`/api/admin/roles` (GET), `/api/admin/roles/{role}/permissions` (PUT PermissionDef `{entity, read, write, create, delete, submit, cancel, export, import, if_owner, read_groups[], write_groups[], where?}`);
`where` limits a rule to records whose fields hold given values, all of them: `{"customer_type": ["Individual"], "disabled": false}` (data, select, check, link, email and int fields; links by record id, an unset checkbox reads as `false`; up to 10 fields and 50 values each). A role can hold several rules for one entity with different conditions (and `if_owner`), and a record is reachable through any of them, in lists, reads, filters, reports and live updates alike;
`/api/admin/packages` (GET, POST `{packages[]}`); `/api/admin/webhooks` (GET, POST `{name, url, topics[]}` → `{id, secret}`, DELETE `/{id}`);
`/api/admin/automations` (GET, POST `{name, entity, event: created|updated|submitted|cancelled|deleted, condition?, actions: [...] , is_active}`, DELETE `/{id}`).
Automation actions: `{"type":"set_fields","values":{field: CEL}}`, `{"type":"create_document","entity":E,"values":{field: CEL}}`, `{"type":"notify","to":"owner|role:<name>|user:<id>","title":CEL,"body"?:CEL}`, `{"type":"email","to":CEL,"subject":CEL,"body":CEL}`.

Customization: `GET /api/customize` → `{customizations: [{id, seq, entity, op, is_active, note, created_at}], warnings}`;
`POST /api/customize` `{entity, op, note?}`; `PUT /api/customize/{id}/active` `{active}`; `GET /api/customize/export`; `POST /api/customize/import`.
Ops: `{"op":"add_field","field":Field,"after"?:name}` (custom field names start with `x_`), `{"op":"set_property","field","property","value"}` (label, description, required, hidden, read_only, in_list, in_filter, default, default_expr, visible_if, required_if, read_only_if, max_length, min, max), `{"op":"add_rule","rule":{name, check: CEL, message, on: save|submit}}`, `{"op":"add_options","field","options":[...]}`, `{"op":"set_entity_property","property":"label|label_plural|description|icon|title_field|list|search_fields|form|numbering","value"}` (`numbering`: `{template, gapless, on: save|submit, date_field}` with `{seq:<width>}` in the template; a gapless series stays gapless), `{"op":"define_entity","entity":EntityDef}` (name starts with `X`), `{"op":"set_workflow","workflow":WorkflowDef}`, `{"op":"set_print_template","template":"<jinja html>"}`.

Plugins (sandboxed WebAssembly, see README): `GET /api/admin/plugins` → `{plugins: [{id, name, version, description, sha256, hooks, fuel_limit, memory_limit_mb, enabled, updated_at}]}`;
`POST /api/admin/plugins` `{name, version?, description?, hooks: [{entity|"*", stage: before_validate|validate|before_submit|on_submit|before_cancel|on_cancel|before_delete}], fuel_limit?, memory_limit_mb?, wasm_base64}` → `{id}` (same name replaces);
`PUT /api/admin/plugins/{id}/enabled` `{enabled}`; `DELETE /api/admin/plugins/{id}`.

`PUT /api/admin/plugins/{id}/config` `{config}` replaces a plugin's settings and secrets; `PATCH` merges the given keys. Plugin endpoints:
any method on `/api/ext/{plugin}/{path}` (signed in) or `/api/public/{tenant}/{plugin}/{path}` (public routes),
and WebSockets at `/ws/ext/{tenant}/{plugin}/{path}` (token in `Authorization` or `access_token` unless public).
Manifest, contract and host calls: [plugins.md](plugins.md).

Notifications (own): `GET /api/notifications`, `POST /api/notifications/{id}/read`.

## Apps — `/api/admin/apps` (administrators; [design](plan/apps.md))

`AppSummary` = `{id, name, version, runtime: native|wasm|meta, summary, categories[], publisher?, verified, icon_url, source: native|upload|store:<url>, private}`.

* `GET /api/admin/apps` → `{installed: [AppSummary + {enabled, installed_at, update: {version}|null}], available: [AppSummary], uploads: off|private|cluster, platform, stores: [{url, ok, error}]}`.
  `installed` also lists native apps whose packages the workspace already has (installed when it was created); `available` holds this build's native apps, releases available on the cluster and the workspace's private uploads. `update` is a newer compatible release on the cluster (from the stores too when `uploads` is `cluster`), or this build's version of a native app.
* `GET /api/admin/apps/store?q=&category=` → `{apps: [AppSummary]}`: matching apps available here first, then the stores' results (cached one minute; a store that does not answer is skipped).
* `GET /api/admin/apps/{id}/plan?version=` → `{app: AppSummary + {description, license, homepage}, steps: [{id, version, action: install|upgrade|already, source: native|cluster|private|store:<url>}], permissions, digest, increases[], warnings[], uninstall: keep|ask}`.
  Steps are in installation order (dependencies first). `permissions` covers everything the plan installs or upgrades (the shape of `ferrum_apps::Permissions`); `increases` lists what upgrades ask for beyond the installed versions; `digest` = SHA-256 over the ordered steps' `(id, version, permissions digest)`. Store bundles are downloaded and verified while planning.
* `POST /api/admin/apps/{id}/install` and `/upgrade` `{version, consent}` (`consent` = the plan's `digest`) → the plan carried out. The plan is computed again: a different digest answers `409 {"error": "conflict", "message", "plan": <fresh plan>}`. Store releases are made available on the cluster (verified: a trusted store countersignature, the listed digest, the role rule), then each app's packages, plugin and record are installed; a failure takes back what the call did in the workspace. `upgrade` needs the app installed; `install` on an installed app applies its plan as well (as after uploading a newer version).
* `POST /api/admin/apps/{id}/disable` / `enable` → 204. Disabling turns the plugin off and takes the app's packages out of the workspace (rows kept; role assignments to the app's roles are dropped); refused while an enabled app requires it. Enabling is refused for a revoked release.
* `DELETE /api/admin/apps/{id}?delete_data=true` → `{removed, deleted_data}`: packages out (with their roles), plugin and its private data removed, record forgotten. Rows in the app's tables are deleted only with `delete_data` **and** `uninstall: ask` in its manifest (`keep` apps never lose data). Refused while another installed app requires it.
* `POST /api/admin/apps/upload` (multipart, field `bundle`, ≤ 50 MB) → `201 {app: AppSummary, plan}`. Verified against `apps.trusted_keys`; unsigned or untrusted bundles only with `apps.allow_unsigned_uploads`. `uploads: private` accepts sandboxed apps without packages, visible only in this workspace; `cluster` makes the app available to every workspace. Native apps, apps for another platform version, older versions than the one available, and roles granting entities the app does not define (the role rule) are refused.

### App stores (what this server reads)

Stores are configured in `apps.registries`. Only an app's `id` and a release's `version` are required; other fields are optional (alternative names in brackets are accepted).

* `GET {store}/v1/apps?q=&category=&platform=<PLATFORM_VERSION>` → `{apps: [...]}` (or `items`, `results`, `data`, or a bare array) of `{id, name, version [latest_version, latest.version], runtime, summary [description], categories, publisher [publisher.id], verified [publisher_verified, publisher.verified], icon_url [icon] (absolute, or relative to the store)}`.
* `GET {store}/v1/apps/{id}` → the same object, optionally wrapped as `{app: {...}}` (used for icons).
* `GET {store}/v1/apps/{id}/releases` → `{releases: [...]}` of `{version, requires {app: semver requirement} [manifest.requires], platform [manifest.platform], digest [sha256], bundle_url [download_url], status, yanked}`; 404 means the store does not have the app. Releases marked `yanked: true` or with status `yanked`, `revoked`, `rejected`, `pending` or `in_review` are ignored. A bundle's own `app.yaml` wins over the listing (requirements and platform are re-checked after download).
* `GET {store}/v1/apps/{id}/releases/{version}/bundle` (unless `bundle_url` says otherwise) → the `.fapp`, redirects followed. It must carry a store signature by a key in `apps.trusted_keys`, and match the listed `digest` when there is one.
* `GET {store}/v1/revocations` → `{"payload": "<JSON text>", "signatures": [{"key_id": "<16 hex>", "sig": "<base64 Ed25519 signature of the payload's UTF-8 bytes>"}]}` with payload `{"generated_at": "<RFC 3339>", "revocations": [{"app", "version"? (all versions when absent or "*"), "reason"?}]}`. At least one trusted store key must have signed it and every signature by one must verify; otherwise the list is ignored (and logged). A list generated before the newest one already applied from that store (a stale cache or a replay) is ignored; the newest time is kept per store in the database, so every replica agrees, and a list more than a day old is logged.

## AI agents (MCP)

`POST /mcp` — Model Context Protocol, Streamable HTTP, stateless (JSON responses, no session id;
`GET`/`DELETE` → 405). Auth as above (`Bearer` session token or API key); 401 carries
`WWW-Authenticate`. Methods: `initialize` (versions 2025-06-18, 2025-03-26, 2024-11-05), `ping`,
`tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`
(`ferrum://overview`, `ferrum://entity/{name}`, `ferrum://record/{entity}/{id}`), `prompts/list`,
`prompts/get`. Tool failures come back as results with `isError: true` and the same error JSON as
the REST API; unknown tools or bad arguments are JSON-RPC errors (-32602). Administrators also get the App Manager's tools — `list_apps`, `plan_app`, `install_app` (consent = the plan's digest, as on the Apps page), `set_app_enabled`, `uninstall_app` — so an agent can extend the workspace the same way a person does.

### OAuth 2.1 (for hosted MCP clients)

Discovery: `GET /.well-known/oauth-protected-resource` (RFC 9728) and
`GET /.well-known/oauth-authorization-server` (RFC 8414); a 401 from `/mcp` carries
`WWW-Authenticate: Bearer resource_metadata="…"`. `POST /oauth/register` (RFC 7591, public clients;
redirect URIs must be https, loopback http — any port — or an app scheme). Authorization endpoint:
the web app's consent page `/app/oauth/authorize` (`response_type=code`, `client_id`, `redirect_uri`,
`code_challenge`, `code_challenge_method=S256`, `state`), which calls `GET /api/oauth/clients/{id}`
and, after the user approves, `POST /api/oauth/authorize` → `{redirect_to}` (adds `code`, `state`,
`iss`). `POST /oauth/token` (form): `grant_type=authorization_code` (`code`, `redirect_uri`,
`client_id`, `code_verifier`) or `grant_type=refresh_token` (`refresh_token`, `client_id`) →
`{access_token, token_type: "Bearer", expires_in: 3600, refresh_token, scope}`. Codes are single-use
(a replay revokes the tokens it produced); refresh tokens rotate and reuse revokes the family.
`POST /oauth/revoke` (`token`). Access tokens work for `/mcp` and the whole REST API.

## Files

`POST /api/files` (multipart: `file`, optional `entity`, `record_id`, `public`; ≤25 MB) → `{id, url: "/files/<id>", file_name, size, sha256}`; `GET /files/{id}` (authenticated download, streamed).

Larger files (up to 2 GB by default, S3-compatible storage only) go straight from the client to the bucket:

1. `POST /api/files/uploads` `{file_name, content_type, size, entity?, record_id?}` → `201 {upload_id, method: "PUT", url, headers, expires_at}`. `409` when the server stores files on a local disk.
2. Send the bytes with that method, URL and headers (valid for an hour).
3. `POST /api/files/uploads/{upload_id}/complete` → `201` and the same body as `POST /api/files`. The server copies the object to the file's own key (later requests to the upload URL cannot change it), checks its size against `size` (a mismatch discards it) and records its SHA-256. `409` if nothing has arrived yet or the upload expired.

Uploads never completed are deleted with their bytes after they expire. `GET /api/version` → `uploads: {max_bytes, direct, direct_max_bytes}` tells clients which way a file goes.
Public files (`public=true`; images and PDFs only — logos, product photos, brochures) are served without signing in at `GET /files/public/{id}` with an immutable cache and a sandboxing CSP. Files live in S3-compatible storage in production.

## System

`GET /api/ready` (DB reachable), `GET /api/version`, `GET /metrics` (Prometheus), `POST /api/tenants` (requires `X-Ferrum-Bootstrap-Token`), `GET /_ping`, `GET /_health`.

## ERPNext compatibility

`GET|POST /api/resource/{DocType}`, `GET|PUT|DELETE /api/resource/{DocType}/{name}` (`name` = document number or id; DocType names map by removing spaces), `GET /api/method/frappe.auth.get_logged_user`. Responses use `{"data": ...}`.
