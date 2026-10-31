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
| `POST /code` | `{tenant, email}` → `{sent: true}`: emails a portal person a six-digit code to sign in with ([plan](plan/apps-ui.md) §9.1). The same answer, after the same time (600 ms at least), whether or not the address belongs to anyone, and nothing is sent to members (they sign in with passwords). A code works once, for ten minutes, takes five wrong tries, and only the newest one works; a person is sent at most five an hour. 503 when the server sends no email |
| `POST /code/sign-in` | `{tenant, email, code, session?}` → as `/login` (with two-factor sign-in on, the second step follows) |
| `GET /me` | → `{tenant, user: {id, email, full_name, is_admin, roles}, default_company, scopes, fiscal_year_start_month, timezone, today, packages, two_factor_setup, accounts}`; `accounts`: in shapes B and C, the accounts host that keeps the person's password, two-factor sign-in and passkeys (their account joined their Ferrum account), else `null` |
| `GET /2fa` | → `{enabled, pending, recovery_codes_left, required}` |
| `POST /2fa/begin` | → `{secret, uri (otpauth://), qr_svg}`: a new secret for an authenticator app (TOTP, SHA-1, 6 digits, 30 s); it takes effect once confirmed |
| `POST /2fa/enable` | `{code}` → `{recovery_codes}` (ten, shown once; and `token`, a full session, when called with a setup-only token) |
| `POST /2fa/recovery-codes` | `{code}` → `{recovery_codes}`: new ones replace the old |
| `POST /2fa/disable` | `{password}` or `{code}` → `204` |
| `POST /logout_all` | revokes every session of the caller (and clears the session cookie) |
| `POST /logout` | signs this browser out: its session ends, with the tokens bound to it, and the cookie is cleared → `204`. Both send `Clear-Site-Data: "cache"` (pages kept by the browser go; what apps keep for working offline stays) |
| `POST /token` | (session cookie) → `{token, token_type, expires_in, tenant, email}`: a bearer token for a page that still sends one, bound to the browser's session |
| `GET /sessions?user=` | → `{sessions: [{id, user_id, actor, created_at, last_seen_at, expires_at, user_agent, ip, current}]}`: the signed-in browsers of the caller (administrators: of `user`) |
| `DELETE /sessions/{id}` | signs one browser out (one's own; administrators: anyone's) |
| `POST /step-up` | `{password, code?}` → `204`: the person proved themselves again; sensitive actions accept the session for ten minutes |
| `POST /passkey/options` | → `{request, publicKey}`: what the browser needs to sign in with a passkey on this host (`navigator.credentials.get`; binary fields base64url) |
| `POST /passkey` | `{request, credential, session?}` → a session, as `/login`: the account the passkey belongs to (on a workspace's own host, only that workspace's); the device must have verified the person |
| `POST /passkeys/options` | → `{request, publicKey}` for `navigator.credentials.create`: a passkey for the caller on this host; `403` unless the session signed in or stepped up in the last ten minutes |
| `POST /passkeys` | `{request, credential, name}` → `201` the passkey kept; `GET /passkeys` → `{passkeys: [{id, name, rp_id, transports, created_at, last_used_at}]}`; `PATCH /passkeys/{id}` `{name}`; `DELETE /passkeys/{id}` |
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
* on a host registered for a workspace, a cookie of another workspace is refused; on a public
  site's host (`site:{app}`) none is accepted, on the portal's host the workspace's is.

**Kinds of people** ([plan](plan/apps-ui.md) §9.1): portal people and guests hold no roles and no
administration, whatever was given them, so `/api/v1/…`, reports and search show them nothing;
they reach what apps answer for them by who they are (the portal's endpoints below).

Document actions may declare `params` (`[{name, label, type: data|text|select|int|date, options?,
required?, description?}]`): the form asks for them, prefilled from the record's fields of the same
name, and sends them as the action's body. An action whose result carries `file: {name, type,
content}` makes the form save that file (e-invoice JSON, say).

## Ferrum accounts — the accounts host (shapes B and C)

With `FERRUM_BASE_DOMAIN` set ([operations](operations.md#many-workspaces-subdomains-and-ferrum-accounts),
[plan](plan/apps-ui.md) §9.3, §12.3), each workspace is at `{slug}.{base}` (and any custom domain it
verified), and people sign in once, at `accounts.{base}`, as their **Ferrum account**: one password,
two-factor sign-in, passkeys and list of browsers for every workspace. A workspace's account joins a
Ferrum account once the person proves it is theirs (its password, a code sent to its address, its
single sign-on, or, with the address already proven by a code, confirming); never by the address
alone. Each host keeps a session of its own.

**The way in.** A workspace's host without a session sends `/login` on to
`https://accounts.{base}/authorize?host={its host}&state={random}&prompt=none` and keeps `state`, with
where to go back to, in the cookie `__Host-ferrum-auth` (ten minutes). Signed in there, with nothing to
ask, the accounts host sends the browser straight back to `https://{host}/_auth/callback?code=…&state=…`:
the code works once, within a minute, on that host only, for a member who is still active (and still
joined); the host checks `state` against its cookie, opens its own session and goes on. Otherwise the
accounts host shows the one step needed: signing in, going on as this account (`prompt=select_account`,
after someone signed out of that host), adding the workspace, its single sign-on (when it requires it of
non-administrators), turning on two-factor sign-in (when it requires it), or no access. A host of
another region's workspace is looked up in the regions of `FERRUM_REGIONS` and the request sent there.

On the accounts host (only there; `404` elsewhere):

| Method & path | Body → response |
|---|---|
| `POST /api/accounts/sign-in` | `{email, password}` → the session cookie (`{cookie: true, …}`), or the next step: `{second_factor: true, ticket}` (an authenticator code, `…/sign-in/second-factor`), or `{verify: true, ticket}`. The first time, with no Ferrum account for the address, a workspace's own password makes one: the account takes that workspace account's password, authenticator app and unused recovery codes, once the code just sent to the address proves it (`…/sign-in/verify`; without a mailer at once, its address unproven). Rate limited per address; 5 failures lock 15 minutes |
| `POST /api/accounts/sign-in/second-factor` | `{ticket, code}` → as `/sign-in` |
| `POST /api/accounts/sign-in/verify` | `{ticket, code}` → the session cookie |
| `POST /api/accounts/code` | `{email}` → `{sent: true}`: a six-digit code to set a password with (a forgotten one, or a first one for someone added without one). Sent only to an address a member of this cluster has, at most five an hour; the same answer after the same time (600 ms) for every address. 503 without a mailer |
| `POST /api/accounts/password` | `{email, code, password}` → as `/sign-in`: sets the account's password (making the account when there is none, its address proven) and ends its other sessions. The authenticator code follows when the account has one |
| `GET /api/accounts/me` | → `{account: {id, email, name, verified, two_factor, home_region, home}, workspaces: [{id, slug, name, host, url, joined, active, admin}], region}`: the account chooser; once the address is proven, also the workspaces with an account for it that has yet to join (`joined: false`). `home_region`, `home`: an account of another region, visiting, and its accounts host there |
| `POST /api/accounts/continue` | `{host, state}` → `{redirect}` back to that host with a code, or `{need, join?}`: the step it needs |
| `POST /api/accounts/join` | `{host, state, password? \| code?, totp?}` → as `/continue`, the workspace's account joined: its password, or a code sent to the address (`/join/code`), or neither when `join.click` (the address is proven and that account has no authenticator app); `totp`, the code of that account's own app, when it has one |
| `POST /api/accounts/join/code` | `{host}` → `{sent: true}`: a code to the account's address, to add that workspace |
| `POST /api/accounts/sso/finish` | `{state, code, binding, next}`: a workspace's single sign-on came back to the accounts host (`https://accounts.{base}/app/login`, the redirect URI registered with the provider); `next` is the `/authorize` request it started from → `{redirect}` back to the workspace's host with a code. The workspace's account joins the Ferrum account signed in here when they share an address |
| `POST /api/accounts/sign-out-everywhere` | → `204`: ends every session of the Ferrum account and of every workspace account it joined, on every device |
| `GET /regions/start?region=&next=` | (page) signs in with a Ferrum account that lives in another region of `FERRUM_REGIONS`: off to its accounts host's `/regions/authorize` with a PKCE challenge (S256), whose verifier and a `state` this host keeps in the cookie `__Host-ferrum-region` (ten minutes) |
| `GET /regions/authorize?redirect_uri=&state=&code_challenge=&code_challenge_method=S256` | (page, the account's home) for a region of its own `FERRUM_REGIONS` only (`redirect_uri` = that region's `{accounts host}/regions/callback`): signing in first if need be, then back with a code bound to the challenge, once, within a minute |
| `GET /regions/callback?code=&state=` | (page, the visited region) checks `state`, takes the code to the home's `POST /api/accounts/region-token` and signs the visiting account in |
| `POST /api/accounts/region-token` | `{code, code_verifier, redirect_uri}` → `{sub, email, name, email_verified, two_factor}` (server to server, home region): who the account is. The visited region keeps a visiting account for it (by region and `sub`), with no password there: its password, two-factor sign-in and passkeys stay at home (`403` there), its address and two-factor sign-in count as home vouches for them (a workspace's two-factor rule included). An account of the visited region with the same address keeps it: the visitor is refused |

`/api/auth/…` works there for the Ferrum account itself: passkeys (made for `accounts.{base}`; signing in
with one at `/api/auth/passkey`), two-factor sign-in, the list of browsers, step-up, changing the password
and `POST /logout` (this browser). API keys are refused (they belong to workspaces), and the account's
sessions and tokens work nowhere but on the accounts host.

On a workspace's host, for someone whose account joined a Ferrum account: signing in with that
workspace's own password or passkey answers `403` with the accounts host's address;
step-up checks the Ferrum account's password (and app); changing the password, two-factor sign-in and
passkeys answer `403` (the Ferrum account keeps them). `POST /logout` signs out of that host only, and
the next sign-in there asks which account. People of the portal still sign in on their host.

`GET /_directory?host=` (any host, no session) → `{region}` when the host is this cluster's (a
workspace's subdomain or custom domain, or the accounts or hooks host), else `404`: how other regions'
accounts hosts and edges find where a workspace lives. Pages' bootstrap carries `api.hooks`, the
origin outside callers use for a workspace's endpoints (`https://hooks.{base}` in shapes B and C, else
`PUBLIC_URL`), which no change of the workspace's addresses moves, and `api.accounts`, the accounts
host (shapes B and C; the bar's account menu links "Your workspaces" there), and `host.bound`: the
host belongs to one workspace (registered, or the server's one). On such a host the bar's runtime
remembers the workspace in the browser, and should the host ever answer for another one (a released
address's next owner), it clears everything the browser kept for the origin (local and session
storage, IndexedDB, caches, service workers) before going on; a shared host (shape A′) serves
several workspaces in turn and is left as it is.

## Metadata — `/api/meta`

* `GET /api/meta` → `{version, packages, modules: [{name, label, icon, order, items: ["EntityName" | "report:<name>"], package}], entities: [{name, label, label_plural, kind, module, icon, submittable, custom, permissions}], warnings}`
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
user may read, `{"notification": {id, title, entity, record_id, kind}}` for the user's notifications and
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
limited per app (1 MB unless the app allows more, at most 32 MB). `x-ferrum-site-api` and
`x-ferrum-site-origin` never reach an app from the caller (see public sites below).

The portal's endpoints, for whoever signed in (portal people included), answering by the address
they signed in with:

* `GET /api/app/helpdesk/portal/tickets?cursor=` → `{tickets: [{id, number, subject, status, label,
  priority, opened_at, updated_at}], next_cursor}`: the tickets raised from that address;
  `POST` `{subject, description, priority?: Low|Medium|High}` → `201` a new one (channel Portal,
  linked to the customer the address belongs to; 20 an hour). `GET …/{id}` → the ticket with
  `description`, `messages: [{id, mine, author, body, sent_at}]` (internal notes never), `resolution`,
  `rating`, `feedback`, `can_reply`, `can_rate`; `POST …/{id}/replies` `{body}` (60 an hour; it
  reopens a ticket waiting on the customer or resolved); `POST …/{id}/rating` `{rating: 1-5,
  feedback?}` once resolved. Another person's ticket is 404.
* `GET /api/app/selling/portal/invoices?cursor=` → `{invoices: [{id, number, date, due_date,
  status, is_return, customer, currency, total, outstanding, overdue}], next_cursor}`: the submitted
  invoices of the customers the address belongs to (their own email, or a contact's; not disabled
  customers, nor a web shop's unchecked guests). `GET …/{id}` → one with `company`, `lines: [{item,
  description, qty, unit, rate, amount}]`, `taxes`, `net_total`, `total_taxes`, `grand_total`,
  `pdf` (whether the server makes PDFs); `GET …/{id}/print[?format=pdf]` → the invoice as the
  workspace prints it, under `Content-Security-Policy: … sandbox`. Drafts, cancelled invoices and
  other customers' are 404.

For Home's My work (`shell.work`): `GET /api/app/helpdesk/work` (open tickets assigned to the
caller, due on their SLA's resolution date) and `GET /api/app/projects/work` (the caller's open
tasks, due on their expected end), each `{items: [{title, subtitle, url, due, priority,
updated_at}]}`.

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
  records page if the person may read the entity: the records of the app it belongs to when that
  app has no interface of its own (`/data/{app}/e/…`), else `/data/e/…` (today's web app for a
  workspace not on the shell); else 403 (404 for an entity the workspace does not have). `GET /open/{entity}` → the same for the list (an app's `shell.open` `list`). The rest of the
  query goes along.
* `GET /{public path}/{workspace}/…` (an app's `shell.public`, such as `/drive/s/acme/{link}` and
  `/meet/guest/acme/{link}`) → the app's page for people outside the workspace: no session, no bar,
  `Referrer-Policy: no-referrer`, `X-Robots-Tag: noindex, nofollow`; 404 when the workspace, the app
  or its page is not there (or the host serves another workspace). Rate-limited per address.
* `GET /data/{app}/…` → the records of an app without an interface of its own (no `ui`, no
  `shell.app`; not a connector, a suite or an add-on): the shell's records frame with the modules of
  its packages and its add-ons'. 403 when the person may read none of them; for an app that has an
  interface now, or is not installed, `e/{entity}[/{id}]` goes to `/open/…` and anything else to
  `/data/`. `/data/e/…`, `/data/r/…` and `/data/customize/…` are the records of any entity, its
  reports and Customize.
* `GET /app/…` → `303` to where an address of today's web app (gone since U4b) lives now, with its
  query: `/app/e/{E}[/{id}]` → `/open/…`, `/app/login` → `/login` (the single sign-on return address
  providers have registered), `/app/admin…`, `/app/apps…` → `/admin/…`, `/store/…`, `/app/mail…` and
  the other moved apps → their pages, anything else → `/`. `/app/sw.js` is a worker that clears
  today's web app's cache and removes itself.
* `GET /healthz` → `ok` (for load balancers).
* `GET /login?next=` → the sign-in page. On the portal's host, and on the way to `/portal/`, it
  offers an emailed code first.
* `GET /portal/` → the workspace's portal ([plan](plan/apps-ui.md) §9.1): the pages the apps give
  this person (`shell.portal`), straight to the only one for a portal person who has one; `GET
  /portal/{id}/…` → that page, the app's interface with `<ferrum-portal-bar>` (the workspace, the
  pages, signing out) instead of the bar and `host.kind: "portal"` with `portal: {home, pages: [{id,
  title, url}]}` in the bootstrap. Signed out → sign-in. Members may look; guests and service
  accounts get 403. A portal person who opens the shell's or an app's page goes to `/portal/`, and
  `/open/{entity}/{id}` to the portal page that shows the record (its `entity` and `open`).
* Hosts by their target (§7.2): a `portal` host has the portal, sign-in, `/open` and the platform's
  files; everything else there leads to `/portal/`. On a `site:{app}` host every page is the app's
  public site: `GET /{path}` goes to the app's site handler as `GET /_site/{path}` (the package named
  like the app first), with `x-ferrum-site-api: /api/site/{workspace}/{package}` and
  `x-ferrum-site-origin: {scheme}://{host}` set by the server; without a session; rate-limited per
  address; with `X-Content-Type-Options`, `Referrer-Policy` and a strict
  `Content-Security-Policy` unless the app sets its own. The API's paths (`/api/…`, `/files/…`)
  work on every host.
* `GET /_ui/{app}/{digest}/{path}` → a file of an interface release: public, `immutable`.
* `GET /_shell/v1/{file}` → the shell's own files (`runtime.js`, `live-worker.js`, …), the newest of
  v1; `GET /_shell/theme/{hash}.css` → the theme.

For the bar, with the cookie or a token:

* `GET /api/shell` → `{v, shell, workspace, person, apps: [{id, app, title, description, icon,
  color, compact, url, family}], create: [{app, label, entity, url}], intents: [{app, action, type,
  title, url}], sidepanels: [{id, app, title, icon, contextual, url}], families: [{id, title, apps}],
  assistant}`: what the caller may use. `family` names the head of the entry's family (`families`
  lists them, heads first). Apps without an interface of their own, without launcher entries of
  their own, get one entry into their records (`/data/{app}/`, the manifest's name and summary) when
  the person may read one of their entities. `ETag`.
* `GET /api/shell/version?app=` → `{app, version, release}`: the interface release the workspace
  runs, so an open tab can tell it is outdated.
* `POST /api/shell/app-token` `{app}` (the session cookie, from the site's own pages) →
  `{token, token_type: "Bearer", expires_in: 300, app}`: an **app token** for an app whose interface
  runs isolated and that the person may use ([plan](plan/apps-ui.md) §12.3). It works only as a bearer
  token on `/api/v1/…`, `/api/meta/…` and `/api/ext/{app}/…`, with the person's rights narrowed to
  what the app declared and never as an administrator; it is bound to the browser session (signing
  that out ends it), refused by every other endpoint, socket and sign-in step, and a request whose
  `X-Ferrum-App` names another app is 403. Requests from an opaque origin (`Origin: null`, an isolated
  frame) to those paths get CORS answers without credentials (`Access-Control-Allow-Origin: *`).

An app whose interface runs isolated (an unsigned upload private to the workspace) has its pages at
its routes as usual, but the page is the bar and a sandboxed frame:
`GET /_isolated/{app}/{digest}/{path}` serves its files, `index.html` only to frames
(`Sec-Fetch-Dest: iframe`, else 403), every answer with `Content-Security-Policy: sandbox …` and
its own files only, `Access-Control-Allow-Origin: *`, `Cross-Origin-Resource-Policy: cross-origin`.
The page's own policy keeps the frame to that release (`frame-src`).

**Origins of their own** (`FERRUM_APPS_BASE`, [plan](plan/apps-ui.md) §7.1 shape D). With an apps
base set, an isolated app of a workspace has the origin `{app}--{workspace}.{apps base}` (when that
makes a hostname label that reads back as the same app and workspace: at most 63 characters, no `--`
in the app's id; any other keeps the sandboxed frame), and its pages frame that origin instead, with
`allow-same-origin` (storage and workers of its own, a site apart from the workspace's). On that
origin every path the server does not keep serves the app's page, with `#ferrum-bootstrap`
`{v, app: {id, version, release, title}, workspace: {slug, name, origin}, oauth: {authorize, token,
client_id, redirect_uri}}`, and its release's files by name; the policy allows `'self'` only and
`frame-ancestors` the workspace's origins. Of the server's paths only `/api/v1/…`, `/api/meta/…`,
`/api/ext/{app}/…`, `POST /oauth/token` and `POST /oauth/revoke` answer there (the rest is 404), and
no cookie is read or set on it.

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

## Administration (administrators only)

Workspace settings: `GET /api/admin/settings` → `{timezone, fiscal_year_start_month, require_two_factor}`;
`PUT` with any of them (`timezone` is an IANA name such as `Asia/Kolkata`; `require_two_factor`
applies from each person's next sign-in; `require_sso` refuses password sign-in to everyone but
administrators and needs an enabled single sign-on provider).

Domains ([operations](operations.md#custom-domains)): `GET /api/admin/domains` → `{domains: [{host,
kind (subdomain or custom), target (shell, portal, site:{app}), is_primary, verify_token, status
(verified, pending, failing, released), verified_at, checked_at, failures, released_at,
created_at}], point_to}`; `POST {host, target}` adds a custom domain (201); `DELETE
/api/admin/domains/{host}` removes one; `PUT …/{host}/primary` makes a working one the main address
of its kind; `POST …/{host}/check` checks it now → `{host, answers}`. Public:
`GET /.well-known/ferrum-domain` answers the token of the custom domain the request reached, and
`GET /_tls/ask?domain=` answers 200 when the edge may get a certificate for that host.

Single sign-on: `/api/admin/sso` (GET → `{providers: [{id, label, issuer, client_id, has_secret,
domains, auto_create, default_roles, enabled, linked_users}]}`; POST `{label, issuer, client_id,
client_secret?, domains[], auto_create, default_roles[], enabled}` → `{id}`, the issuer's discovery
document fetched to check it), `/api/admin/sso/{id}` (PUT the same — `client_secret` left out keeps
the stored one, `""` clears it; DELETE unlinks the people linked through it).

`/api/admin/apps/{id}/screenshots/{n}?version=` (GET: an uploaded app's `screenshots[n]` from its
bundle, for the install dialog; plans list them as `app.screenshots`, store apps' from the store, and
say `app.isolated` when the interface will run isolated);
`/api/admin/users` (GET, POST `{email, full_name, password?, roles[], is_admin, kind?}` → `{id, kind}`; `kind` is `member` (the default), `portal`, `guest` or `service`, and portal people and guests take no roles), `/api/admin/users/{id}/kind` (PUT `{kind}`: signs the person out everywhere), `/api/admin/users/{id}/roles` (PUT `{roles}`),
`/scopes` (PUT `{entity, ids[], default?}`), `/active` (PUT `{active}`), `/password` (PUT `{password}`), `/two-factor/reset` (POST: turns the person's two-factor sign-in off, lifts a lockout and signs them out everywhere — for a lost phone), `/sso/unlink` (POST: forgets the identity at a single sign-on provider the account is linked to); the user list shows `kind`, `two_factor`, `sso` and `locked`;
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

## Apps — `/api/admin/apps` (administrators; [design](plan/apps.md))

`AppSummary` = `{id, name, version, runtime: native|wasm|meta, summary, categories[], publisher?, verified, icon_url, source: native|upload|store:<url>, private}`.

* `GET /api/admin/apps` → `{installed: [AppSummary + {enabled, installed_at, update: {version}|null}], available: [AppSummary], uploads: off|private|cluster, platform, stores: [{url, ok, error}]}`.
  `installed` also lists native apps whose packages the workspace already has (installed when it was created); `available` holds this build's native apps, releases available on the cluster and the workspace's private uploads. `update` is a newer compatible release on the cluster (from the stores too when `uploads` is `cluster`), or this build's version of a native app.
* `GET /api/admin/apps/store?q=&category=` → `{apps: [AppSummary]}`: matching apps available here first, then the stores' results (cached one minute; a store that does not answer is skipped).
* `GET /api/admin/apps/{id}/plan?version=` → `{app: AppSummary + {description, license, homepage}, steps: [{id, version, action: install|upgrade|already, source: native|cluster|private|store:<url>}], permissions, digest, increases[], warnings[], uninstall: keep|ask}`.
  `description` is the opening prose of the app's description file (its README) as plain text, about 600 characters at most, without its HTML, pictures, badges, headings, tables or code (`ferrum_apps::description::excerpt`).
  Steps are in installation order (dependencies first). `permissions` covers everything the plan installs or upgrades (the shape of `ferrum_apps::Permissions`); `increases` lists what upgrades ask for beyond the installed versions; `digest` = SHA-256 over the ordered steps' `(id, version, permissions digest)`. Store bundles are downloaded and verified while planning.
* `POST /api/admin/apps/{id}/install` and `/upgrade` `{version, consent}` (`consent` = the plan's `digest`) → the plan carried out. The plan is computed again: a different digest answers `409 {"error": "conflict", "message", "plan": <fresh plan>}`. Store releases are made available on the cluster (verified: a trusted store countersignature, the listed digest, the role rule), then each app's packages, plugin and record are installed; a failure takes back what the call did in the workspace. `upgrade` needs the app installed; `install` on an installed app applies its plan as well (as after uploading a newer version).
* `POST /api/admin/apps/{id}/disable` / `enable` → 204. Disabling turns the plugin off and takes the app's packages out of the workspace (rows kept; role assignments to the app's roles are dropped); refused while an enabled app requires it. Enabling is refused for a revoked release.
* `DELETE /api/admin/apps/{id}?delete_data=true` → `{removed, deleted_data, leftovers}`: packages out (with their roles), plugin and its private data removed, record forgotten. Rows in the app's tables are deleted only with `delete_data` **and** `uninstall: ask` in its manifest (`keep` apps never lose data). Refused while another installed app requires it. `leftovers` are the apps its installation brought in (a suite's members, what it required) that nothing installed needs now, in an order they can be removed in; an app an administrator installed for itself is never among them.
* `GET|PUT|DELETE /api/admin/apps/{id}/dev-ui` (development servers only, `FERRUM_DEV_UI=1`; `404` elsewhere) → `{app, dev_ui}`: `PUT {url}` serves the installed app's pages in this workspace from its interface's Vite server on this machine (`http://localhost:<port>`, `127.0.0.1` or `[::1]`), behind the real session, bar and bootstrap, with hot reload (`ferrum-app dev --ui`); an isolated interface stays isolated, framed from there. The pages' release is `dev` (no "new version" notices), and their policy allows that server's files and socket. `DELETE` serves the release again; a restart forgets them all.
* `POST /api/admin/apps/upload` (multipart, field `bundle`, ≤ 50 MB) → `201 {app: AppSummary, plan}`. Verified against `apps.trusted_keys`; unsigned or untrusted bundles only with `apps.allow_unsigned_uploads`. `uploads: private` accepts sandboxed apps without packages, visible only in this workspace; `cluster` makes the app available to every workspace. Native apps, apps for another platform version, older versions than the one available, and roles granting entities the app does not define (the role rule) are refused.

### App stores (what this server reads)

Stores are configured in `apps.registries`. Only an app's `id` and a release's `version` are required; other fields are optional (alternative names in brackets are accepted).

* `GET {store}/v1/apps?q=&category=&platform=<PLATFORM_VERSION>` → `{apps: [...]}` (or `items`, `results`, `data`, or a bare array) of `{id, name, version [latest_version, latest.version], runtime, summary [description], categories, publisher [publisher.id], verified [publisher_verified, publisher.verified], icon_url [icon] (absolute, or relative to the store)}`.
* `GET {store}/v1/apps/{id}` → the same object, optionally wrapped as `{app: {...}}` (used for icons).
* `GET {store}/v1/apps/{id}/releases` → `{releases: [...]}` of `{version, requires {app: semver requirement} [manifest.requires], platform [manifest.platform], digest [sha256], bundle_url [download_url], status, yanked}`; 404 means the store does not have the app. Releases marked `yanked: true` or with status `yanked`, `revoked`, `rejected`, `pending` or `in_review` are ignored. A bundle's own `app.yaml` wins over the listing (requirements and platform are re-checked after download).
* `GET {store}/v1/apps/{id}/releases/{version}/bundle` (unless `bundle_url` says otherwise) → the `.fapp`, redirects followed. It must carry a store signature by a key in `apps.trusted_keys`, and match the listed `digest` when there is one.
* `GET {store}/v1/revocations` → `{"payload": "<JSON text>", "signatures": [{"key_id": "<16 hex>", "sig": "<base64 Ed25519 signature of the payload's UTF-8 bytes>"}]}` with payload `{"generated_at": "<RFC 3339>", "revocations": [{"app", "version"? (all versions when absent or "*"), "reason"?}]}`. At least one trusted store key must have signed it and every signature by one must verify; otherwise the list is ignored (and logged). A list generated before the newest one already applied from that store (a stale cache or a replay) is ignored; the newest time is kept per store in the database, so every replica agrees, and a list more than a day old is logged.

## Search — `GET /api/search?q=&limit=`

Records of every entity the caller may read (masters and documents with a title or search fields),
found by title and search fields as lists search, ranked by how well the title matches, then by how
recently the record changed: `{results: [{kind: "record", entity, id, title, subtitle, url, updated_at}],
pages, partial, took_ms}`; a record with neither a title nor a number is left to its app's own
search. `pages` ("Go to") are the lists the caller may open whose name matches, at most five:
`{kind: "page", entity, title, subtitle, url: "/open/{entity}", icon}`. At least two characters; at most 100; `limit` up to 50 (default 20). `partial` is
true when an entity took longer than its time budget and was left out. The bar's search asks this
and the apps' `shell.search` providers together; a provider answers `GET {endpoint}?q=&limit=` with
`{results: [{title, subtitle?, url, icon?, updated_at?}]}`, each `url` a page of this site the caller
may open.

## Notifications — `/api/notifications` (the caller's own)

Each notification has a kind: the platform's `mention`, `assignment`, `comment`, `watch`,
`automation` and `general`, and the apps' (`shell.notification` in app.yaml) as `{app}.{id}`. Each
person chooses per kind how it reaches them: `inapp` (in the bar), `email` (in the bar and by email
at once), `digest` (in the bar, and in one daily email of those still unread once the oldest has
waited a day), `push` (in the bar, and on each device where they turned notifications on; below)
or `off`. Links in emails open the record (`/open/{entity}/{id}`) on the workspace's own address,
else on `PUBLIC_URL`.

| Request | |
|---|---|
| `GET /api/notifications?limit=` | `{notifications: [{id, user_id, title, body, entity, record_id, kind, delivery, read_at, emailed_at, created_at}], unread}`, newest first; `limit` up to 100 (the default), 0 for the count alone |
| `POST /api/notifications/{id}/read`, `POST /api/notifications/read-all` | marks one, or all, read (204) |
| `GET /api/notifications/types` | `{types: [{kind, label, app, default, channel}], channels, email, push}`: the kinds the caller may choose for (apps' only for apps they may use), each with its default and their choice; `email` is false when the server sends no email, `push` when it pushes to no devices (no secret key) |
| `PUT /api/notifications/preferences` `{kind, channel}` | how one kind reaches the caller; `channel: null` returns it to the default. `email` and `digest` need a mailer, `push` the secret key |

An app sends its kinds with `ferrum_kernel::notify::send_kind(tx, tenant, user, "{app}.{id}",
default, title, body, entity, record)`, `default` being what its manifest declares
(`notify::declared_default`): `email` when the declared list has email, else `push`, else
`digest`, else `inapp`; an empty list is `off`.

### Notifications on a device — `/api/push` (docs/plan/apps-ui.md §12.5)

Web Push (RFC 8030, 8291, 8292): the bell's settings turn notifications on for the device in
use, and kinds set to `push` then pop up there through the core worker (`/sw.js`), even with no
Ferrum page open. A device's subscription belongs to its browser session: signing that device
out, signing out everywhere, or a person made inactive ends it. Each message is encrypted for the
device (the push service cannot read it) and carries `{id, title, body, url, unread}`; the worker
shows it, puts `unread` on the installed app's icon, and a click marks it read and opens `url`
(`/open/{entity}/{id}`, else Home). The cookie of a browser session is needed; tokens of API
clients are refused (422).

| Request | |
|---|---|
| `GET /api/push` | `{key, here, devices}`: the server's VAPID key (base64url, the browser's `applicationServerKey`; `null` when it pushes to no devices), whether this device has notifications on, and on how many of the caller's devices they are |
| `PUT /api/push` `{endpoint, keys: {p256dh, auth}}` | this device gets them, through this subscription (`PushSubscription.toJSON()`) with its browser's push service (204). Refused (422) for an endpoint that is not a push service the server sends to, or keys that are not a subscription's; 403 for someone acting as another person |
| `DELETE /api/push` | this device gets no more (204) |

The page bootstrap carries `push: {key, on}` for signed-in pages (`null` without push), so the bar
knows without asking. Browsers' own services are allowed (Chrome's and Edge's, Firefox's, Safari's,
Windows'); operators may list others (`FERRUM_PUSH_SERVICES`, docs/operations.md).

### Installing — `/manifest.webmanifest`, `/sw.js`

| Request | |
|---|---|
| `GET /manifest.webmanifest` | the workspace as an app to install (`id: /`, its name, Ferrum's icons); asked with the page's cookie, else for the workspace the host belongs to |
| `GET /manifest.webmanifest?app={route}` | an app that asks to be installable on its own (`installable: true` on its `shell.app` entry), for someone who may use it: `id`, `scope` and `start_url` `/{route}/`, its launcher tile as the icon; else 404 |
| `GET /_shell/app-icon.svg?icon=&color=&letter=&mask=` | an app's launcher tile as an image (a Lucide icon of the bar's, else the letter, on the colour); square with `mask=1` |
| `GET /sw.js` | the core worker, scope `/`: no fetch handler, so it adds nothing to page loads; registered only when someone turns notifications on |

## My work — `GET /api/work`

What waits for the caller, from the platform: `{groups: [{id, title, items}], partial}` with the
groups `assigned` (open assignments: `{kind: "assignment", id, assignment, title, subtitle, url,
due, priority, at}`), `approvals` (records in a workflow state that a transition only some roles
may take leaves, the caller holding one of those roles, not their own records when the transition
needs a second person, its condition met: `{kind: "approval", id, title, subtitle, url, actions,
at}`) and `mentions` (unread: `{kind: "mention", id, notification, title, subtitle, url, at}`), at
most 50 each. `partial` is true when a source failed and was left out. `url` opens the record
(`/open/{entity}/{id}`).

Home asks the apps' `shell.work` providers (`{id, title, endpoint}` in app.yaml) itself, in
parallel, each within 5 seconds: `GET {endpoint}` answers `{items: [{title, subtitle?, url, due?,
priority?, updated_at?}]}`, `url` a page of this site, `due` a date (`YYYY-MM-DD`, the day it is
due in the person's calendar), `priority` `High` to flag it. Home lists everything together, most
pressing first: overdue, due today, decisions, mentions, then by due date and recency.

## Record cards — `GET /api/cards/{entity}/{id}`

A record's preview (the bar shows it when a link to a record is hovered or focused), derived from
its metadata for those who may read it: `{entity, id, title, kind, number, icon, state, fields:
[{name, label, type, value}], updated_at, url}`. `state` is the workflow state (the initial one for
records from before their workflow), else Draft/Submitted/Cancelled for submittable records;
`fields` are up to six the list shows, not the title, a link as the linked record's title, masked
as reads are. Cached by the browser for 30 seconds.

An app may serve its entities' cards itself (`shell.card: [{entity, endpoint}]`): the bar then asks
`GET {endpoint}?id=` and draws `{title, kind?, number?, icon?, state?, fields?: [{label, value,
type?}], updated_at?}`; the endpoint checks the caller's access itself. `type` (`date`,
`datetime`, `currency`, `decimal`, `int`, `percent`, `check`) says how the bar formats `value`.
Links the bar previews: `/open/{Entity}/{id}`, and a records frame's `…/e/{Entity}/{id}`; an
element marked `data-ferrum-nocard` (a list, whose rows already show the fields) has none.

## The assistant — `/api/assistant` ([operations](operations.md#the-assistant))

The signed-in person's conversations with the assistant (cookie session; app screens may not call
these). Off unless the server has `FERRUM_ASSISTANT_API_KEY`.
* `GET /api/assistant` → `{available, reason, admin, threads: [{id, title, state, updated_at}]}`
  (the 50 most recent; `state` is `idle`, `running` or `waiting`).
* `POST /api/assistant/threads` → 201 `{id}`.
* `GET /api/assistant/threads/{id}` → `{id, title, state, live, draft, pending, messages}`:
  messages as the panel draws them (`{seq, role, items}`, items of kind `text`, `thinking`, `tool`,
  `result`); `pending.calls` are the changes waiting for a decision.
* `POST /api/assistant/threads/{id}/messages {text, context?}` and
  `POST /api/assistant/threads/{id}/decide {allow}` start a turn and answer with its events
  (`text/event-stream`, one JSON object per `data:` line): `message` (a stored message), `delta`
  (`kind` `text` or `thinking`), `tool` (`status` `running`, `done`, `error`), `confirm`
  (`pending`), `note`, `restart` (discard the text so far: the call is tried again), `resync`
  (reload the thread), and last `done` (`state`) or `error` (`message`). The turn goes on if the
  request ends. 409 while another turn runs on the thread, or when nothing waits for a decision;
  422 for an empty or overlong message or a conversation that is full; 429 past the hourly
  allowance.
* `GET /api/assistant/threads/{id}/live` follows a turn under way from this replica (first event
  `draft`: what it has said so far); 404 when it runs elsewhere.
* `POST /api/assistant/threads/{id}/stop` → 204; `DELETE /api/assistant/threads/{id}` → 204.
* The shell API's `ferrum.shell.assistant(context)` opens it beside a page with what the person is
  looking at (`{entity, id, title}`); `/api/shell` says `assistant: true` and lists the panel when
  the person may use it.

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
`GET /oauth/authorize` (`response_type=code`, `client_id`, `redirect_uri`, `code_challenge`,
`code_challenge_method=S256`, `state`), a consent page the server draws for the person signed in
with the browser's cookie (anyone else signs in first and comes back). It refuses a client that is
not registered or an address it did not register, and sends nothing there. Its form answers with
`POST /oauth/authorize` (same fields, `decision=allow|deny`, the page's own origin only) → `303` to
the redirect URI with `code`, `state` and `iss`, or with `error=access_denied`. A page of its own
may use `GET /api/oauth/clients/{id}` and `POST /api/oauth/authorize` → `{redirect_to}` instead. `POST /oauth/token` (form): `grant_type=authorization_code` (`code`, `redirect_uri`,
`client_id`, `code_verifier`) or `grant_type=refresh_token` (`refresh_token`, `client_id`) →
`{access_token, token_type: "Bearer", expires_in: 3600, refresh_token, scope}`. Codes are single-use
(a replay revokes the tokens it produced); refresh tokens rotate and reuse revokes the family.
`POST /oauth/revoke` (`token`). Access tokens work for `/mcp` and the whole REST API. Refresh tokens
end with the person's sessions: after signing out everywhere, a new password or deactivation, a
refresh token issued before is refused and its family revoked.

**Apps' own clients.** `client_id=app:{app}:{workspace}` needs no registration: the authorization
endpoint makes it for the signed-in person's own workspace, an app installed there whose interface
runs isolated with an origin of its own and that they may use, and its one redirect URI
`{app origin}/_ferrum/callback`. Its access tokens are app tokens (`expires_in: 300`, narrowed as
above), the answer carries `user: {email, name}`, and the token endpoint answers `invalid_grant` once
the app is gone or the person may no longer use it.

## Files

`POST /api/files` (multipart: `file`, optional `entity`, `record_id`, `public`; ≤25 MB) → `{id, url: "/files/<id>", file_name, size, sha256}`; `GET /files/{id}` (authenticated download, streamed).

Larger files (up to 2 GB by default, S3-compatible storage only) go straight from the client to the bucket:

1. `POST /api/files/uploads` `{file_name, content_type, size, entity?, record_id?}` → `201 {upload_id, method: "PUT", url, headers, expires_at}`. `409` when the server stores files on a local disk.
2. Send the bytes with that method, URL and headers (valid for an hour).
3. `POST /api/files/uploads/{upload_id}/complete` → `201` and the same body as `POST /api/files`. The server copies the object to the file's own key (later requests to the upload URL cannot change it), checks its size against `size` (a mismatch discards it) and records its SHA-256. `409` if nothing has arrived yet or the upload expired.

Uploads never completed are deleted with their bytes after they expire. `GET /api/version` → `uploads: {max_bytes, direct, direct_max_bytes}` tells clients which way a file goes.
Public files (`public=true`; images and PDFs only — logos, product photos, brochures) are served without signing in at `GET /files/public/{id}` with an immutable cache and a sandboxing CSP. Files live in S3-compatible storage in production.

## System

`GET /api/ready` (DB reachable), `GET /api/version`, `GET /metrics` (Prometheus), `POST /api/tenants` (requires `X-Ferrum-Bootstrap-Token`; a slug any workspace ever used is taken for good), `GET /_ping`, `GET /_health`.

## ERPNext compatibility

`GET|POST /api/resource/{DocType}`, `GET|PUT|DELETE /api/resource/{DocType}/{name}` (`name` = document number or id; DocType names map by removing spaces), `GET /api/method/frappe.auth.get_logged_user`. Responses use `{"data": ...}`.
