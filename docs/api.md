# Ferrum HTTP API

Base URL: the server root (dev `http://127.0.0.1:5150`). JSON everywhere. A live, tenant-specific
OpenAPI 3.1 document is at `GET /api/openapi.json` (it includes every customization).

## Conventions

| Topic | Rule |
|---|---|
| Auth | `Authorization: Bearer <session token>` from `/api/auth/login`, or `Bearer fk_<prefix>_<secret>` (API key) |
| Money & decimals | **Strings** (`"1234.5"`), never JSON floats. Send strings too; numbers are accepted but avoid floats |
| Dates | `YYYY-MM-DD`; date-times RFC 3339 UTC (`2026-10-28T10:15:00.000Z`). Business dates ("today", posting dates and times, defaults) are local to the workspace's time zone (`GET /api/auth/me` → `timezone`, `today`); pages show times in `display_timezone`, the person's own zone (the `timezone` preference) or else the workspace's |
| Links | UUID strings. Add `titles=true` to list/get to receive `_titles: {field: "Display title"}` on records and child rows. As in ERPNext, a title shows even when the caller cannot open the linked record (only its title field or number, nothing else). A record headed by a link (an appointment by its patient) is named, and found by `search`, by the linked record's title, two steps on at most, when that entity is open; through a restricted one it shows its number |
| Concurrency | Records carry `version`; responses send `ETag: "v<version>"`. Send `If-Match: "v<version>"` on update/delete/submit/cancel; a mismatch → `409` |
| Idempotency | `POST /api/v1/{entity}` accepts `Idempotency-Key` (8–200 chars). A replay returns the original record with `200` and `Idempotent-Replayed: true` |
| Errors | `{"error": code, "message": text, "fields"?: [{"field", "message", "row"?: [tableField, rowIndex], "path"?: [[tableField, rowIndex], [tableField, rowIndex]]}], "error_id"?}` (`path` only on an error in a line's own line, below, whose `row` is then its line); codes: `validation_failed` (422), `invalid` (422), `not_found` (404), `conflict` (409), `forbidden` (403), `unauthorized` (401), `too_many_requests` (429), `retry`/`unavailable` (503, with `Retry-After`) |
| Document status | `docstatus`: 0 draft, 1 submitted, 2 cancelled. Submitted documents are immutable; cancel + amend to change |

## Auth — `/api/auth`

| Method & path | Body → response |
|---|---|
| `POST /login` | `{tenant, email, password}` → `{token, token_type: "Bearer", expires_in}` (rate limited; 5 failures lock the user 15 min). With two-factor sign-in on → `{second_factor: true, ticket, expires_in: 300}`; in a workspace that requires it, for someone who has yet to set it up → `{two_factor_setup: true, token, expires_in: 900}`, a token good only for `/me` and `/2fa` (setup) |
| `GET /sso?tenant=` | → `{providers: [{id, label}]}`: the workspace's single sign-on buttons (none for an unknown workspace) |
| `POST /sso/start` | `{tenant, provider, redirect_uri, binding}` → `{url}` to send the browser to. `redirect_uri` is the sign-in page (registered with the provider, which sends the browser nowhere else); `binding` is a random secret (16–200 characters) the browser keeps to finish |
| `POST /sso/finish` | `{state, code, binding}` (what the provider sent the browser back with) → `{token, …}`. Each sign-in works once, within ten minutes, in the browser that started it. The provider's own two-factor rules apply instead of Ferrum's |
| `GET /saml/{workspace id}` | The workspace's SAML 2.0 metadata (`application/samlmetadata+xml`), for identity providers to load: its entity ID (this address) and its assertion consumer service. 404 for a workspace that is not active |
| `POST /saml/{workspace id}/acs` | Where a SAML provider posts its answer (`SAMLResponse`, `RelayState`; HTTP-POST binding). A good answer → `303` to the `redirect_uri` the sign-in started with, plus `state` and `code`, which finish it with `/sso/finish` as above; anything else → a page that says why. For SAML providers `redirect_uri` must be an address of this server's own (`PUBLIC_URL`, a host of the workspace, the accounts host) |
| `POST /login/second-factor` | `{ticket, code}` (six digits from the authenticator app, or a recovery code) → `{token, …}`. Each app code is accepted once; wrong codes count toward the same lockout as wrong passwords |
| `POST /code` | `{tenant, email}` → `{sent: true}`: emails a portal person a six-digit code to sign in with ([plan](plan/apps-ui.md) §9.1). The same answer, after the same time (600 ms at least), whether or not the address belongs to anyone, and nothing is sent to members (they sign in with passwords). A code works once, for ten minutes, takes five wrong tries, and only the newest one works; a person is sent at most five an hour. 503 when the server sends no email |
| `POST /code/sign-in` | `{tenant, email, code, session?}` → as `/login` (with two-factor sign-in on, the second step follows) |
| `GET /me` | → `{tenant, user: {id, email, full_name, is_admin, roles}, default_company, scopes, fiscal_year_start_month, timezone, display_timezone, today, packages, two_factor_setup, accounts}`; `accounts`: in shapes B and C, the accounts host that keeps the person's password, two-factor sign-in and passkeys (their account joined their Ferrum account), else `null` |
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
| `GET /api_keys`, `POST /api_keys` `{name, expires_at?, scope?}` → `{id, key}`, `DELETE /api_keys/{id}` | API keys (secret shown once). `scope: "dav"` makes an app password for calendar and contacts apps: it signs them in at `/api/dav/…` over HTTP Basic (email and password) and is refused as a bearer token. `scope: "records:Customer=read;SalesInvoice=read+create"` makes a records key: it reaches only `/api/v1/…` and `/api/meta/…`, with its owner's rights narrowed to the entities and actions it names (`read`, `write`, `create`, `delete`, `submit`, `cancel`, `export`, `import`), never more than theirs, never an administrator's, and no field group; an unknown entity or action is a `422` |

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

**Kinds of people** ([plan](plan/apps-ui.md) §9.1): guests hold no roles and no administration,
whatever was given them, so `/api/v1/…`, reports and search show them nothing. Portal people hold
only the roles marked for the portal ([builder decisions](builder/decisions.md) BD-02), whose rules
reach only their own records: `/api/v1/…`, the record API, search and Studio show them those, and
never a record's comments, files, assignments, tags or history. Both reach what apps answer for them
by who they are (the portal's endpoints below), and portal people Studio's generic portal page
(`GET /api/app/studio/portal/entities` → `{entities: [{name, label, label_plural, description,
permissions}]}`, what their portal roles reach).

Document actions may declare `params` (`[{name, label, type: data|text|select|int|date, options?,
required?, description?}]`): the form asks for them, prefilled from the record's fields of the same
name, and sends them as the action's body. An action whose result carries `file: {name, type,
content}` makes the form save that file (e-invoice JSON, say).

`POST /api/auth/sso/{provider}/backchannel-logout` takes `logout_token` as a form field, from the
provider ([OpenID Connect Back-Channel Logout](operations.md#sign-in-security)): `200`, or `400
{error: "invalid_request"}` when the token is refused.

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
where to go back to, in a cookie of that sign-in's own, `__Host-ferrum-auth-{the state's first 12
characters}` (ten minutes), so tabs that set off together do not undo each other. Signed in there, with nothing to
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

* `GET /api/meta` → `{version, packages, modules: [{name, label, icon, order, items: ["EntityName" | "report:<name>"], package, description?, color?, create?, assistant?: {instructions, suggestions?}, portal?}], entities: [{name, label, label_plural, kind, module, icon, description, submittable, custom, permissions}], warnings}` (`description`: what the record type is for, which the help center at `/data/help/` searches). A workspace's own menus (`define_module`, below) are among `modules` with `package: "tenant"` and their own `description`, `color`, `create` and `assistant`; a package's module has none of those.
* `GET /api/meta/{Entity}` → full description used to render forms and lists:
  `{name, label, label_plural, kind (master|document|child|settings|ledger), module, title_field, submittable, mergeable, linked_from: [{entity, label, field, field_label}] (the lists the caller may read that link here; `field` is a filter path, so `items.item` for a child table's row), company_scoped, numbering, list: {columns, sort}, form: [{label, fields, columns (1-4), collapsible, tab?, visible_if?}], search_fields, rules, actions: [{name, label, docstatus[], permission, confirm?, condition?, read_only?, params?, group?, roles?, refuse_if?, effects? (administrators only), may_run}], workflow?: {initial_state, states: [{name, docstatus, edit_roles, editable? (the only fields a person changes in the state), required? (what it needs to be entered and saved)}], transitions: [{action, from, to, roles, condition?, not_creator, not_creator_of? (links, `field` or `table.field`, whose records the person taking it may not have raised), may_take (whether the caller may take it, as taking it asks)}]}, fields: [Field], children: {tableField: {entity, label, fields: [Field]}}, permissions: {read|write|create|delete|submit|cancel|export|import: "none"|"own"|"all"}}`
  An action's `may_run` says whether the caller may press it at all: its permission or a role's rule naming it, and its `roles`, as running it checks them (an administrator always may); `condition`, `docstatus` and `refuse_if` are judged per record, so a form evaluates them on the record it shows. A configured action's `effects` (what it writes) go to administrators only.
* `Field`: `{name, label, type, required?, unique?, read_only?, hidden?, in_list?, in_filter?, options?, to?, of?, precision?, currency?, min?, max?, max_length?, default?, default_expr?, computed?, rollup?, visible_if?, required_if?, read_only_if?, read_only_message?, fetch_from?, description?, widget?, link_filters?, storage: "column"|"ext", can_read, can_write}`.
  `rollup` (`{table, op: sum|count, field?}`) is an exact total over the record's own rows of `table`: the sum of a number field of the rows, or their count. The server computes it at every save and in the draft preview, before validation, in exact decimals; a value sent for it is ignored.
  Types: `data text markdown code int decimal currency percent date datetime check select link table json email phone url attach color secret`.
  `visible_if`/`required_if`/`read_only_if` are CEL over `doc` (and `parent` in child rows); the server is the authority. `read_only_if` is judged on the record (or row) as stored before the change: while it holds, the field ignores input exactly as a `read_only` field does for that caller (the system and an amendment's copy still write it); a new record or row has nothing stored, so only `read_only` counts for it, and a condition that cannot be evaluated counts as holding. A field may say why instead: with `read_only_message` ("went to the kitchen: void it and order again"), a person's value that differs from the stored one while `read_only_if` holds refuses the save with 422 and that message on the field (and the row, for a line); both values are compared after normalising, so a form sending the whole record back unchanged passes. The system, an amendment's copy and a field that is `read_only` outright still ignore the input silently; table and secret fields do not take the message.
  A link field to its own entity makes a hierarchy: a record cannot be its own parent, and a move that would make a loop is refused on the field naming both records (`Asia cannot sit under India: India is already under Asia, which would make a cycle in the hierarchy`).
  `widget` says how forms edit a field where its type alone does not: `studio_query` on a `json` field holding a Studio query, which forms build with Studio's report builder and show the first rows of (the field's own rules still decide what it may hold).
  CEL also sees `user` and `today`, `now`, `time` (local to the workspace). A record's whole numbers are signed integers, as literals are (`doc.days * 24`, `doc.owed > -100`); decimals are floats for comparison.
  **Linked records** (docs/builder/decisions.md BD-14): `links.<field>` is the record a link field of the record names, as an expression sees a record (its fields, and its lines in their order, so `links.type.documents.all(d, doc.documents.exists(r, r.document == d.document && r.file != ''))`), or null when the field is empty or the record is gone. It is read as the workspace, whoever saves or presses, so a check does not pass or fail by who runs it; its secret fields are left out; it is read without a lock in the save's transaction, so a rule sees the linked record as it is when the save runs, a change earlier in the same transaction included. One level: a link of the linked record is an id. `links` works in entity rules (not a line's rules), an action's `condition` and `refuse_if`, workflow guards, automation conditions and action values, and service targets' expressions; in `default_expr`, `computed`, `visible_if`, `required_if`, `read_only_if` and a section's `visible_if` it is refused when saved (they are worked out without a transaction, and in the browser). Checked when saved: each `links.<name>` is a link field of the entity, at most 5 different ones in one expression, and `links` is read only as `links.<name>` (`links['type']` is refused). Each place loads the names its expressions use once (a save with three rules over `links.type` reads the type once). The approvals inbox takes a guard that reads `links` to hold; taking the step decides. A form cannot judge such a condition, so it leaves the button to the server.
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
| `POST /api/v1/{E}/{id}/actions/{action}` | custom actions from metadata (e.g. `make_delivery_note`), implemented by an app or set up by configuration (Customization, "Configured actions"); body = params; returns action JSON (often `{entity, id}` of a created record). An answer without a `message` gets one from what it says: the record's new `status` and the records it made ("Now Admitted; made STU-2026-00001"). A `read_only` action runs without locking or auditing, in a read-only transaction |
| `POST /api/v1/{E}/{id}/workflow/{action}` | workflow transition. One naming `roles` is taken by a holder of one of them on a record they can read; one naming none by whoever may change the record (write permission, and its scope admits the record); an outside person (portal, guest) takes only one naming a role of theirs. `403` otherwise. Bulk, MCP, capabilities (`…transition.<action>`) and processes ask the same |
| `POST /api/v1/{E}/_calc` | preview: runs defaults, fetches and calculation hooks on an unsaved body (include `id` to start from a saved record) → computed document, nothing written. Use for live totals |
| `GET /api/v1/{E}/{id}/print` | HTML print format; `?format=pdf` → the same as a PDF (`application/pdf`; `503` when the server has no PDF renderer, see `GET /api/version` → `pdf`) |
| `POST /api/v1/{E}/{id}/email` | `{to, subject?, message?, pdf?: true}` → `202 {queued, to, subject, attachments}`: emails the document through the server's mailer, its print format attached as a PDF (stored on the record), and notes it on the record's timeline. Queuing the mail raises `<Entity>.emailed` on the outbox in the same transaction (docs/events.md), so webhooks and apps' event sinks hear of each mail that goes: the event's `actor` is who sent it, its payload `{entity, id, event: "emailed", actor, request_id, to, subject, pdf, attachments}` (`pdf` whether the PDF went along; `attachments` the stored files). Automations do not take it. Needs export permission on the entity; 30 per user per minute. `503` without a mailer, or for a PDF when the server has no PDF renderer or its mailer cannot attach (`GET /api/version` → `mail`, `mail_attachments`, `pdf`) |
| `GET /api/v1/{E}/{id}/audit` | `{events: [{at, actor, actor_name, action, version, diff}], chain_valid}` |
| `GET /api/v1/{E}/_export` | CSV (export permission), streamed a page at a time with every row however many; or Excel with `format=xlsx` (field labels as headers, numbers as numbers), at most 100,000 rows and refused (422) past them rather than cut short; same filters as list |
| `POST /api/v1/{E}/_import` | JSON array (≤5000) → `{created, ids, errors: [{row, error, fields?}]}` (`207` if some failed) |
| `POST /api/v1/{E}/_bulk` | one operation on a selection: `{ids: [...]}` or `{filters: [...]}` (≤500 records) plus `{"op": "update", "values": {...}}`, `{"op": "delete"}`, `{"op": "submit"}`, `{"op": "cancel"}`, `{"op": "workflow", "action"}`, `{"op": "action", "action", "params"?}`, `{"op": "assign", "user", "note"?, "due_date"?, "priority"?}`, `{"op": "add_tags" \| "remove_tags", "tags"}` → `{succeeded, failed, results: [{id, ok, error?}]}`. Each record runs in its own transaction with the caller's permissions; `error` has the API error shape |
| `POST /api/v1/{E}/_bulk/jobs` | the same over up to 100,000 records, or all a filter matches when it starts → `202 {job}`; see [Background jobs](#background-jobs--apijobs) |
| `GET /api/v1/{E}/_duplicates?limit=` | a mergeable entity's likely duplicates: `{groups: [{field, value, ids}]}` (needs delete) |
| `POST /api/v1/{E}/{id}/merge` | `{duplicate, reason, values?, version?}`: moves everything linked to the duplicate onto this record → `{keep, merged, links_moved}` (needs delete). Whole or not at all, and not undone. The old id then answers with this record. `values` are what was chosen from the duplicate to stay, written onto this record in the merge's transaction (at `version`). The duplicate's own lines stay with it: to keep a contact's second email, send this record's `emails` with that one added in `values`. Mergeable among the shared packages: Customer, Supplier and Contact (likely duplicates by `email`, `phone`, `full_name`). |

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

## Shared views — `/api/views/{Entity}`

A list view a team shares: a question, not an answer, so what each person gets from it is still what
they may read. `GET` → `{views: [{id, name, value, version, by, can_change}]}` for anyone who can read
the entity; `POST` `{name, value}` shares one (`201`; `409` if the list already has that name, in any
case; `value` is an object of at most 16 KB, and a list keeps at most 200). `PUT .../{id}`
`{name, value, version}` and `DELETE .../{id}` are for whoever shared it or an administrator; a
`version` someone else has moved past is a `409`.

## Preferences — `/api/prefs` (the signed-in user's own)

`GET /api/prefs` → `{prefs: {scope: {value, version}}}`; `PUT /api/prefs/{scope}` `{value, version?}` →
`{scope, value, version}` (a scope is 1-120 of `A-Z a-z 0-9 : _ . -`, e.g. `list:SalesInvoice`; values
≤64 KB; sending the `version` you read makes a newer write elsewhere a `409`); `DELETE /api/prefs/{scope}`
resets it. At most 500 scopes per user. Two scopes the server reads: `locale` (`{language}`, the
interface's language) and `timezone` (`{zone}`, an IANA name such as `Asia/Kolkata`, refused
otherwise: the zone times are shown in for that person; business days stay the workspace's).

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

Operations, the warehouse and the shop floor document their endpoints at the top of
`crates/ops/src/site.rs`, and in `addons/wms/src/site.rs` and `addons/mes/src/lib.rs` of the ERP
repository. A scanner sends what it read (`code`): a lot, a barcode or an item code, matched exactly.
IoT Edge documents its own at the top of `src/site.rs` in [its repository](https://github.com/CBFerrum/ferrum-app-edge): gateways, definitions tried on a
sample, flows simulated, devices, alarms and robots, and `POST /api/site/{tenant}/edge/ingest` for a
device that talks to the platform directly, with `x-ferrum-device-token`.

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
  app has no interface of its own (`/data/{app}/e/…`), else the records of the first workspace menu (in order) that
  lists it (`/data/{menu}/e/…`), else `/data/e/…` (today's web app for a
  workspace not on the shell); else 403 (404 for an entity the workspace does not have). `GET /open/{entity}` → the same for the list (an app's `shell.open` `list`), except for a
  settings entity that is not per company: its one record once it exists. The rest of the
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
* `GET /data/{menu}/…` → the records of a workspace's own menu (`define_module`), the same frame with
  `scope: {app: "{menu}", title: <label>, packages: ["tenant"], modules: ["{menu}"]}` (pages show
  the menu named in `modules`; `packages` is for an older interface). 403 when the person reads none
  of its entities; `/data/{menu}` redirects to `/data/{menu}/`.
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
  like the app first; a handler says it serves such pages with `Site::serves_pages`, and only those
  apps may be a domain's target), with `x-ferrum-site-api: /api/site/{workspace}/{package}` and
  `x-ferrum-site-origin: {scheme}://{host}` set by the server; without a session; rate-limited per
  address; with `X-Content-Type-Options`, `Referrer-Policy` and a strict
  `Content-Security-Policy` unless the app sets its own. The API's paths (`/api/…`, `/files/…`)
  work on every host.
* `GET /_ui/{app}/{digest}/{path}` → a file of an interface release: public, `immutable`.
* `GET /_shell/v1/{file}` → the shell's own files (`runtime.js`, `live-worker.js`, …), the newest of
  v1; `GET /_shell/theme/{hash}.css` → the theme.

**Keys the bar keeps.** The bar takes Ctrl/Cmd+K (its search) before a page's own handlers see it,
except inside an element that claims it: `data-ferrum-keys="mod+k"` on the editable region or any
element around it (a space-separated list; `mod` is Cmd on a Mac, Ctrl elsewhere; elements in a
shadow root count). There the key is the page's (an editor's "insert link") and the search is the
bar's button. `mod+k` is the only key a page can claim today.

For the bar, with the cookie or a token:

* `GET /api/shell` → `{v, shell, workspace, person, apps: [{id, app, title, description, icon,
  color, compact, url, family}], create: [{app, label, entity, url}], intents: [{app, action, type,
  title, url}], sidepanels: [{id, app, title, icon, contextual, url}], families: [{id, title, apps}],
  assistant}`: what the caller may use. `family` names the head of the entry's family (`families`
  lists them, heads first). Apps without an interface of their own, without launcher entries of
  their own, get one entry into their records (`/data/{app}/`, the manifest's name and summary) when
  the person may read one of their entities. A workspace's own menu gives a member who reads one of
  its entities a tile `{id: "menu-{name}", app: "{name}", title, description, icon, color, compact:
  false, url: "/data/{name}/"}`, and a `create` row `{app: "{name}", label: <the entity's label>,
  entity, url: "/data/{name}/e/{Entity}/new"}` for each of its `create` entities the person may
  create. Reading decides; there is no list of roles. `ETag`.
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

## Background jobs — `/api/jobs`

What takes longer than a request, or must survive a restart ([how they behave](operations.md#background-jobs)).

| Route | |
|---|---|
| `GET /api/jobs?limit=` | newest first (≤200): the caller's own, everyone's for an administrator → `{jobs: [{id, kind, status, attempts, max_attempts, priority, progress, result, last_error, cancel_requested, created_by, created_at, started_at, finished_at}]}` |
| `GET /api/jobs/{id}` | one of those; `404` to anyone but whoever asked for it and administrators |
| `POST /api/jobs/{id}/cancel` | at once if it has not started; within 5 seconds if it has, and its open transaction rolls back |
| `POST /api/jobs/{id}/retry` | a `failed` job, with its attempts back; it still runs as whoever asked |
| `POST /api/admin/export` | administrators: queues the workspace's export (one at a time) → `202 {job}` |
| `GET /api/jobs/{id}/parts/{part}` | administrators: one zip of a finished export (`result.parts[].id`); each download is on the audit trail |

`status` is `queued`, `running`, `done`, `failed` or `cancelled`. A job runs with the rights its owner
has when it runs, not when they asked. A bulk job's `result` is `{total, succeeded, failed, failures}`
(the first 1,000 failures); its `progress` says how far it has got.

## Administration (administrators only)

Workspace settings: `GET /api/admin/settings` → `{timezone, fiscal_year_start_month, require_two_factor}`;
`PUT` with any of them (`timezone` is an IANA name such as `Asia/Kolkata`; `require_two_factor`
applies from each person's next sign-in; `require_sso` refuses password sign-in to everyone but
administrators and needs an enabled single sign-on provider).

Domains ([operations](operations.md#custom-domains)): `GET /api/admin/domains` → `{domains: [{host,
kind (subdomain or custom), target (shell, portal, site:{app}), is_primary, verify_token, status
(verified, pending, failing, released), verified_at, checked_at, failures, released_at,
created_at}], point_to, sites: [{app, title}]}`, `sites` being the apps here with a public site;
`POST {host, target}` adds a custom domain (201), a `site:{app}` only for one of those; `DELETE
/api/admin/domains/{host}` removes one; `PUT …/{host}/primary` makes a working one the main address
of its kind; `POST …/{host}/check` checks it now → `{host, answers}`. Public:
`GET /.well-known/ferrum-domain` answers the token of the custom domain the request reached, and
`GET /_tls/ask?domain=` answers 200 when the edge may get a certificate for that host.

Single sign-on: `/api/admin/sso` (GET → `{providers: [{id, label, protocol, issuer, client_id, has_secret,
saml_metadata, domains, auto_create, default_roles, enabled, linked_users}], saml}`, `saml` being
`{entity_id, acs_url, metadata_url}` for SAML providers' administrators, or `null` when the server
cannot do SAML; POST `{label, issuer, client_id, client_secret?, domains[], auto_create,
default_roles[], enabled}` → `{id}`, the issuer's discovery document fetched to check it; or
`{label, protocol: "saml", saml_metadata, domains[], …}` with the provider's metadata as XML or the
https address it is published at, checked now and read again at each sign-in, `domains` required),
`/api/admin/sso/{id}` (PUT the same — `client_secret` left out keeps the stored one, `""` clears it;
a provider keeps its protocol; DELETE unlinks the people linked through it).

`/api/admin/apps/{id}/screenshots/{n}?version=` (GET: an uploaded app's `screenshots[n]` from its
bundle, for the install dialog; plans list them as `app.screenshots`, store apps' from the store, and
say `app.isolated` when the interface will run isolated);
`/api/admin/users` (GET, POST `{email, full_name, password?, roles[], is_admin, kind?}` → `{id, kind}`; `kind` is `member` (the default), `portal`, `guest` or `service`; guests take no roles, portal people only portal roles, and neither is an administrator), `/api/admin/users/{id}/kind` (PUT `{kind}`: signs the person out everywhere), `/api/admin/users/{id}/roles` (PUT `{roles, until?}`: `until` maps a role given to the moment it ends, a moment still to come; the list gives `role_until` the same way),
`/scopes` (PUT `{entity, ids[], default?}`), `/active` (PUT `{active}`), `/password` (PUT `{password}`), `/two-factor/reset` (POST: turns the person's two-factor sign-in off, lifts a lockout and signs them out everywhere — for a lost phone), `/sso/unlink` (POST: forgets the identity at a single sign-on provider the account is linked to); the user list shows `kind`, `two_factor`, `sso` and `locked`;
`/api/admin/personal-data?email=&phone=&name=` (GET: every record the installed packages declare as the person's — `personal_data` in package manifests, `{entity, match: <email field; the phone field where records hold no address; a link to a record another declaration finds; or `table.field`, people held in a child table's rows, whose copy and erasure are those rows alone>, phone?: <phone field>, name?: <name field>, erase: [fields], where?: {field: value}}` — with its data (for one matched by a table's rows, `table` and `rows`, and those rows alone), and their login account; with `phone`, also the records whose phone field has the same digits however written; with `name`, the records holding that name, whole or as one of the names a field lists, are listed apart under `possibly`, for a person to judge, as are, with `phone`, those whose number differs from it only by a country code) and `/api/admin/personal-data/erase` (POST `{email, phone?, reason}`, reaching what the phone number reaches too, never what a name alone does: blanks the declared fields, a placeholder where a value is required, and empties declared child tables and JSON fields; redacts those fields in the records' audit history, each event keeping its hash and recording the redaction; anonymizes and closes the login account; deletes the workspace's stored exports, and an export being made starts again; refused for administrators and oneself) → `{erased: [{entity, id}], retained, held, account_erased, audit_events_redacted, files_deleted, exports_deleted, apps: [{app, erased}], not_searched}` (what apps keep in tables of their own is under `apps` in the copy as well);
`/api/admin/roles` (GET; each role with `portal`), `/api/admin/roles/{role}/portal` (PUT `{portal}`: marks a role for the portal, refused while a rule of it reaches every entity, deletes, cancels, exports or imports, or reaches every record other than to read reference data, an entity holding no personal data; portal people hold only such roles), `/api/admin/roles/{role}/permissions` (PUT PermissionDef `{entity, read, write, create, delete, submit, cancel, export, import, if_owner, read_groups[], write_groups[], where?, via?, actions?}`);
`via` makes a rule reach the holder's own records through a path to their email (`patient.email`, `student.guardians.email`: at most two links, then optionally a child table, then an email or text field), checked against the entities it crosses when saved and on every use; `actions` names the entity's actions its holders may run on the records it reaches, whatever the action's own permission asks (its condition and its app's checks still apply);
`where` limits a rule to records whose fields hold given values, all of them: `{"customer_type": ["Individual"], "disabled": false}` (data, select, check, link, email and int fields; links by record id, an unset checkbox reads as `false`; up to 10 fields and 50 values each). A role can hold several rules for one entity with different conditions (and `if_owner`), and a record is reachable through any of them, in lists, reads, filters, reports and live updates alike;
`/api/admin/packages` (GET, POST `{packages[]}` → `{installed, kept_newer: [{name, version, build}]}`: a package the workspace holds newer than this build's (a server rolled back) is left as it is, its version, roles and seed data untouched, and listed in `kept_newer`; the others install; `ferrum task doctor` reports it as `packages.newer`); `/api/admin/webhooks` (GET, POST `{name, url, topics[]}` → `{id, secret}`, DELETE `/{id}`; a workspace's own webhook receives a record event's payload with `doc` and, on a change, `was` next to it; an app's webhook receives neither, only which record);
`/api/admin/automations` (GET, POST `{name, entity, event: created|updated|submitted|cancelled|deleted|scheduled, schedule?, condition?, actions: [...] , is_active}`, DELETE `/{id}`).
An automation's `condition` (CEL) sees the record as `doc`; on a change of an existing record (`updated`, `submitted`, `cancelled`) it also sees `old`, the record before the change, so `doc.status == 'Closed' && old.status != 'Closed'` holds once. The event's payload carries `doc` (up to 256 KiB) and, with it, `was`: the previous value of each field the change replaced (not tables), left out when the two together pass 256 KiB.
An automation fires on a record's event, or on time: with `event: "scheduled"` it carries a `schedule` (and only then), in one of two forms.
`{"relative": {"field": F, "offset"?: "-P3D", "at"?: "09:00", "repeat"?: "P1D"|"P1W"|"P1M"|"P1Y", "until"?: G}}` fires for each record at its date or datetime field `F` plus `offset` (a signed ISO-8601 duration of days, hours and minutes: `-P3D`, `PT2H`, `-P1DT30M`; at most five years either way); a date field's moment is at `at` (default 09:00) in the workspace's time zone, a datetime field's at its own time. With `repeat` it fires again every day, week, month or year after that (a month after 31 January is 28 February, then 31 March), up to the record's date or datetime field `G` (`until`, a date counting whole) when given.
`{"every": {"period": "day"|"week"|"month", "at": "HH:MM", "on"?: 1-7 (a week, Monday 1, default 1) | 1-28 or "last" (a month, default 1), "filter"?: [[field, op, value], ...]}}` fires at each period's moment for every record that passes `filter` (list-filter form over the entity's own fields, literal values, at most 8 conditions, all of them).
Moments are found within a minute by a scan on every server, and each (record, moment) fires once however many look. A new automation, or one whose schedule or entity changes, or one switched on again, starts from now: no moment already past fires. A moment fires with the record as it is then, read with the rights of whoever saved the automation: it does not fire if the record is gone, its date has moved away or been cleared, or it no longer passes the filter, and `condition` is judged on it then. A date moved to a new moment fires again for that moment; one moved into the past does not. After the scan has been stopped for more than seven days, the older moments are skipped (logged with how many), not fired late. A scheduled run's changes count one step in an event chain, as an event's do. Each moment queues an `<Entity>.scheduled` event carrying `{automation, record, due_at}`, which webhooks subscribed to the entity's events also receive.
Automation actions: `{"type":"set_fields","values":{field: CEL}}`, `{"type":"create_document","entity":E,"values":{field: CEL}}`, `{"type":"update_linked","link":F,"values":{field: CEL}}`, `{"type":"notify","to":"owner|role:<name>|user:<id>","title":CEL,"body"?:CEL}`, `{"type":"email","to":CEL,"subject":CEL,"body":CEL,"template"?:"<jinja html>"}`, `{"type":"invoke","capability":ID,"input":{name: CEL},"key"?:CEL}` (another app's step through the capability gate, below, "Calling another app's steps").
Each action after the first sees what the ones before it gave as `results` (as a configured action's effects do, below).
**Effects on lines**: `{"type":"append_line","table":T,"values":{field: CEL}}` adds one line at the end of the record's table field `T`; `{"type":"set_line","table":T,"where":CEL,"values":{field: CEL}}` changes the one line for which `where` holds, an expression over `row` (the line) besides `doc`, `user`, `params`, `results` and `links`, which its values also see. No line, or more than one, refuses the run and says which (`set_line: no line of reviews has row.stage == params.stage`; `set_line: lines 3, 4 of reviews all have …`). Both write through the record's ordinary save: one new version per effect, computed fields, rollups, row rules, checks, Logic rules and the audit; every other line keeps its id and values, and the line changed keeps the fields not named. A submitted record refuses them (`a submitted Application cannot be edited; cancel and amend it instead`), and the cap on lines holds. Checked when saved: `T` is a table field of the entity, and each field a field of its lines that an effect may write. They are the only effects that change a table.
`update_linked` writes the record the link field `F` of the triggering record names: that record is locked and saved through its own pipeline (its computed fields, checks, Logic rules and unique keys; its events and automations follow), with the rights of whoever saved the automation, as `set_fields` is (a read-only field of it is not written). Its values see `doc` (the triggering record) and `linked` (the record written, as it was before). An empty link does nothing; a link naming a record that is not there fails the run. `F` must be a link field of the entity and each field written a field of the linked entity, checked when the automation is saved; the chain rule holds, the linked record's change being one step further in the chain.
An e-mail's `body` gives text: it is escaped and its line breaks kept, so nothing a record holds (typed into a public form, say) becomes markup in a mail sent in the workspace's name. For formatted mail, a `template` (MiniJinja, the engine and limits of print templates, at most 64 KB) makes the HTML in place of `body`: its own markup stays, every value it prints is escaped unless the template itself marks it `safe` (never do that with text people typed), and it sees `doc` (the record) and `user` (whom the automation runs as), as the expressions do; one that does not compile is refused when the automation is saved. The `ferrum.mail` capability takes text only.
A refused action is reported as a field error on `actions` with the action's place as its row, the same `row` that errors in a document's child rows carry: `{"field": "actions", "message": "...", "row": ["actions", 1]}` is the second action (counting from 0), so an editor can show the message on that action.
`POST /api/admin/automations/preview {entity, body?, template?, record_id?}` (administrators only) renders what an e-mail action would send, as HTML, without sending or writing anything: the `template` when given, otherwise `body` (an expression) as escaped text. It uses the record `record_id` names, read with the caller's rights (not found or not readable: 404), or when none is named the most recently changed record the caller may read, or an empty record when the entity has none. The answer is `{"html": "...", "record_id": "<id>" | null}`. A template that does not compile or fails, or an expression that fails, is a 422 field error on `template` or `body` with the engine's message; a template over 64 KB is refused as when saving.

Customization: `GET /api/customize` → `{customizations: [{id, seq, entity, op, is_active, note, created_at, superseded_by?}], warnings}` (`superseded_by`, on an active customization the schema no longer applies, the id of the one that replaced it: a later active `add_action` or `remove_action` of the same action, a later active `define_module` or `remove_module` of the same menu, a later active `publish_list` or `unpublish_list` of the same list, or a later active `redefine_entity` of its entity, below);
`POST /api/customize` `{entity, op, note?}`; `PUT /api/customize/{id}/active` `{active}`; `GET /api/customize/export`; `POST /api/customize/import`.
Ops: `{"op":"add_field","field":Field,"after"?:name}` (custom field names start with `x_`; on a package's master, document or settings entity the field may be a table of a custom child entity, below), `{"op":"set_property","field","property","value"}` (label, description, required, hidden, read_only, in_list, in_filter, default, default_expr, visible_if, required_if, read_only_if, read_only_message, max_length, min, max), `{"op":"add_rule","rule":{name, check: CEL, message, on: save|submit}}`, `{"op":"add_options","field","options":[...]}`, `{"op":"set_entity_property","property":"label|label_plural|description|icon|title_field|list|search_fields|form|numbering","value"}` (`numbering`: `{template, gapless, on: save|submit, date_field}` with `{seq:<width>}` in the template; a gapless series stays gapless), `{"op":"define_entity","entity":EntityDef}` (name starts with `X`; a master, a document or a child entity; a master or document may have table fields whose rows are a custom child entity, each child entity the rows of one table field only, and such a child entity may itself have table fields whose rows are another custom child entity (lines within lines: a record, its lines, their lines, and no third level, refused as `lines go two levels deep (a record, its lines, their lines), and XC2 has lines of its own`), and unique keys, `unique: true` on a field or `unique: [[fields]]`, which a child entity may not; a save that would repeat another record's key is refused with 409 naming the fields; a cancelled document and empty values hold no key; defining a key over records that already share a value is refused, listing them), `{"op":"set_workflow","workflow":WorkflowDef}`, `{"op":"set_print_template","template":"<jinja html>"}`, `{"op":"redefine_entity","entity":EntityDef}` (below), `{"op":"add_action","action":ActionDef}` (a configured action, below), `{"op":"remove_action","name"}` (below), `{"op":"define_module","module":{...}}` and `{"op":"remove_module","name"}` (a workspace's own menu, below), `{"op":"add_target","target":TargetDef}` and `{"op":"remove_target","name"}` (service targets, below), `{"op":"publish_list","list":{...}}` and `{"op":"unpublish_list","name"}` (published lists, below).

**Configured actions.** A workspace adds an action to an entity with `add_action` when the action carries `effects`, so it needs no handler: on a custom entity under any name, on a packaged one named `x_…`. It is an ordinary action (`describe` lists it, the record form shows it as a button with its `condition`, `confirm` and `params` dialog, `POST /api/v1/{E}/{id}/actions/{name}` runs it, a role's rule may name it in `actions`), with three more keys:

```json
{"entity": "XRiskAcceptance", "op": {"op": "add_action", "action": {
  "name": "accept", "label": "Accept It", "condition": "doc.status == 'Draft'", "confirm": "Accept taking this risk on?",
  "roles": ["Risk Approver"],
  "params": [{"name": "note", "label": "Note", "type": "text"}],
  "refuse_if": [{"when": "doc.risk_owner == user.email", "message": "a risk's owner does not approve taking it on"}],
  "effects": [
    {"type": "set_fields", "values": {"status": "'Accepted'", "approver": "user.email", "decided_on": "today"}},
    {"type": "update_linked", "link": "risk", "values": {"treatment": "'Accept'"}},
    {"type": "notify", "to": "owner", "title": "'Accepted: ' + params.note"}]}}}
```

* Who may run it: the permission it names (`write` by default) on the record, or a role's rule naming it, as for any action; then, when `roles` is given, one of those roles (an administrator always may), else 403 `'Accept It' is for Risk Approver`; then `condition` (otherwise 422, as today); then each `refuse_if` in order, whose first `when` that holds (CEL over `doc`, `user`, `params` and `links`) refuses with 403 and its `message`.
* `params` are the action's parameters (the request body, an object), `params.<name>` in its expressions.
* `effects` are automation actions (`set_fields` on the record itself, `update_linked`, `create_document`, `notify`, `email`, `invoke`, `append_line` and `set_line` on its lines), run in order inside the action's one transaction, as the system on the runner's behalf, each through the save pipeline every write uses: the record's computed fields, rollups, checks, Logic rules, workflow rules and unique keys apply to what they write, and the usual record events and automations follow after commit. `set_fields` writes read-only fields (a status moved only by buttons); records written say the runner changed them. Each effect sees the record as the effects before it left it. Any refusal (a check, a rule, a key, a missing linked record) undoes the whole action and reaches the caller in its own words. The audit trail records `action:<name>` with the runner and the parameters.
* What an effect may write: a custom entity's fields, a packaged entity's `x_` fields (what a Logic derivation may fill), and a packaged entity's fields a person may type; never the platform's columns (the reserved names `id`, `tenant_id`, `version`, `docstatus`, `number`, `created_at`, `created_by`, `updated_at`, `updated_by`, `amended_from`, `workflow_state` and the like), a computed or rolled-up field, a secret, a table (except one line of it through `append_line` or `set_line`), a package's read-only field or a field in a permission group. No effect runs another action.
* Checked when the customization is saved, each refusal naming the entity, action and effect: expressions compile, fields and links exist where they are written, the above holds; at most 20 configured actions on an entity, 10 effects and 10 `refuse_if` on an action; a configured action is not `read_only`, and `roles` and `refuse_if` go with `effects`. A later customization that would leave one writing a field that is gone is refused.
* The answer is `{status?, made?}`: the record's `status` after it, and `{entity, id, number}` of each record `create_document` made, from which the usual message is derived ("Now Accepted; made a Log"). An e-mail's recipient, subject, body and template all see `params` (`{{ params.note }}` in a template).
* Each effect after the first sees what the ones before it gave as `results`: `results[n]` is the nth effect's (counting from 0), `{entity, id, number}` for `create_document` and `update_linked`, the receipt's result for `invoke`, null for the others. So "create it, then keep its id here" is two effects: `{"type": "set_fields", "values": {"invoice": "results[0].id"}}`.
* A value that is only a path to a value (`doc.fee`, `linked.price`, `links.plan.price`, `row.amount`, `params.x`, `results[0].total`: no operator, no call, no brackets around it) copies it as stored, so an empty number, date or tick stays empty, where any other expression reads an empty number as 0 and an empty tick as false (`doc.fee + 0` writes 0); this holds for `set_fields`, `create_document`, `update_linked`, `append_line` and `set_line`, in automations too.
* Changing one: an `add_action` named as an action an earlier customization added replaces it where it stands among the entity's actions; the earlier one stays in the change list, marked `superseded_by`, and applies again if the later one is switched off. An action a package brought is not replaced ("action lock comes with the package of Shop"). Saving the same operation again in a batch is refused as before.
* Removing one: `{"entity": "XTask", "op": {"op": "remove_action", "name": "x_stop"}}` removes an action a customization added; refused for a package's action and for a name the entity does not have. Switching it off brings the button back. A role's rule naming a removed action still loads and allows nothing.
* Change lists, export and import, packs (which carry them as customizations and require the packages whose entities their effects create), conflicts, upgrade impact and the batch endpoint treat `add_action` and `remove_action` as any customization (neither replaces something a package says, so neither parts ways with an upgrade).

**Worked example: permit stages** (the paper measurement's G6, G12 and G16; `crates/kernel/tests/links_it.rs`). A permit type lists `documents` and `stages` as lines; an application links to it (`type`) and holds `documents` and `reviews`. A rule lodges it only with every document the type lists, a button passes a stage by appending a review, and approval (a workflow transition) waits until the reviews cover every stage of the type:

```json
{"rules": [{"name": "documents", "message": "every document the permit type lists, each with a file",
  "check": "!doc.lodged || links.type == null || links.type.documents.all(d, doc.documents.exists(r, r.document == d.document && r.file != ''))"}]}
{"name": "x_pass", "label": "Pass Stage", "condition": "doc.lodged && links.type != null",
 "params": [{"name": "stage", "label": "Stage", "type": "data", "required": true}, {"name": "note", "label": "Note", "type": "data"}],
 "refuse_if": [{"when": "!links.type.stages.exists(s, s.stage == params.stage)", "message": "the permit type has no such stage"}],
 "effects": [{"type": "append_line", "table": "reviews", "values": {"stage": "params.stage", "reviewer": "user.email", "day": "today", "note": "params.note"}}]}
{"action": "approve", "from": "Draft", "to": "Approved", "roles": ["Officer"],
 "condition": "links.type.stages.all(s, doc.reviews.exists(r, r.stage == s.stage))"}
```

A clerk who cannot open permit types lodges and is refused exactly as an administrator is. Recording an inspection's findings later is `{"type": "set_line", "table": "reviews", "where": "row.stage == params.stage", "values": {"findings": "params.findings"}}`.

**Calling another app's steps (`invoke`).** A configured action's effect or an automation's action may call a capability of the workspace's catalog (docs/logic/capabilities.md): an entity's record operations (`<package>.<entity>.read`, `list`, `create`, `update`, `delete`, `submit`, `cancel`), its package's actions that are listed (an app's contracted ones among them), its workflow's transitions (`<package>.<entity>.transition.<action>`), `ferrum.notify`. It runs through the gate, the owning app's own code and checks, in the action's or automation's one transaction, as the system on the runner's behalf (an automation: as whoever saved it): a refusal by the app's rules or hooks, or by any later effect, refuses the whole press, and nothing of it is kept, the app's postings included.

```json
{"type": "invoke", "capability": "erp.sales_invoice.create",
 "input": {"values": "{'customer': doc.customer, 'items': [{'item_code': 'PERMIT-FEE', 'qty': 1, 'rate': doc.fee}]}", "submit": "true"},
 "key": "'permit-fee:' + doc.id"}
```

* `input` names the capability's inputs, each a CEL expression over `doc`, `user`, `params` and `results`. `GET /api/admin/capabilities` lists them (below). `<entity>.create` takes `values` (the record, lines as a list) and `submit`: `true` creates and submits it in one step, so a document and its postings are one effect with one receipt.
* Checked when saved: the capability is in the workspace's catalog (otherwise "erp.sales_invoice.create is not a capability here: is its app installed?"), its input names are the capability's (an unknown one is refused, a required one must be there), and it is not a configured action (an effect does not run another one, through the gate either). A capability that goes after that (its app removed) refuses the press with the same message; the action stays and the workspace's other customizations still save. An automation is checked when saved through `/api/admin/automations`; one installed from a pack is checked when it runs.
* **Once.** A capability that changes something runs once per effect key, its receipt kept with the change. Without `key`, the key is the action's or automation's own: `action:<Entity>.<action>:<record id>:v<the record's version when pressed>:<the effect's place>` (`automation:<automation id>:<record id>:v<version>:<place>`), so the same press of the record as it is (a double click, a retry after a lost answer) does it once, and a press after the record has changed does it again. With `key` (an expression, e.g. `'permit-fee:' + doc.id`), it happens once for that key for good: a later press runs the effects again, but the step answers with what it did the first time (the record as it is now, for `results`) and changes nothing, so the invoice is raised once. The same key with other input (the fee changed since) refuses the press with 409 `input_changed`: "the key permit-fee:… was used for erp.sales_invoice.create with other input: use a new key for a new request". Whoever pressed is told so in plain words, with no key or capability id ("This step (Create a Sales Invoice) was already done for this record with other details, so it was not done again."), a portal person too; an administrator gets that followed by the precise refusal, which also goes to the server's log, and the gate itself, for processes and agents, keeps the precise refusal.
* A read (`read`, `list`, a read-only action) needs no key; its result is there for the next effect as `results[n]`.

A permit's fee invoiced in ERP once, its id kept on the application:

```json
{"entity": "XPermitApplication", "op": {"op": "add_action", "action": {
  "name": "invoice_fee", "label": "Invoice the Fee", "condition": "doc.status == 'Submitted' && doc.fee_invoice == null",
  "roles": ["Permit Officer"],
  "effects": [
    {"type": "invoke", "capability": "erp.sales_invoice.create", "key": "'permit-fee:' + doc.id",
     "input": {"values": "{'customer': doc.customer, 'items': [{'item_code': 'PERMIT-FEE', 'qty': 1, 'rate': doc.fee}]}", "submit": "true"}},
    {"type": "set_fields", "values": {"fee_invoice": "results[0].id", "status": "'Fee Invoiced'"}}]}}}
```

The invoice is ERP's own: its numbering, taxes, checks and postings are ERP's code, and the builder never writes a ledger line (docs/builder/decisions.md BD-05, BD-13). `erp.sales_invoice.create` stands for whatever the installed ERP's catalog names it.

A workspace's own menu gives a business built by configuration what an app's manifest gives an app: a launcher tile, a sidebar group, quick create and the assistant's notes:

```json
{"entity": "x_grc", "op": {"op": "define_module", "module": {
  "name": "x_grc", "label": "Governance and Risk", "description": "Risks, controls, policies and findings",
  "icon": "clipboard-list", "color": "#7c3aed", "order": 0,
  "items": ["XRisk", "XControl", "XPolicy", "XFinding", "report:risk_heatmap"],
  "create": ["XRisk", "XFinding"],
  "assistant": {"instructions": "A Risk is scored on likelihood and impact…", "suggestions": ["What are our highest risks?"]}}}}
```

* `name`: `x_` then lowercase letters, digits and underscores, at most 40 in all; a package's modules never take `x_` names. The customization's `entity` is the menu's name (whatever is sent). A later `define_module` of the same name replaces the earlier one where it stands, and the list marks the earlier `superseded_by` it; `{"op": "remove_module", "name": "x_grc"}` removes it (refused for a name the workspace has no menu of).
* `label` (required, at most 60 characters), `description` (at most 200), `icon` (a short name such as `clipboard-list`; the bar shows the label's first letter for a name it does not know), `color` (`#rrggbb`), `order` (with the packages' modules).
* `items`: entities (not line entities) and `report:<name>` entries, in order, at most 50, none twice. Naming an entity the workspace does not have is refused; one that goes later (its app uninstalled) drops out of the menu with a schema warning, and the rest stays.
* `create`: the entity items the bar's Create menu offers. `assistant`: `instructions` (at most 2000 characters) the assistant is given on the menu's pages (`/data/{name}/…`), and up to 6 `suggestions` (each at most 200) it offers there.
* `portal: true` shows the menu in the workspace's portal too (docs/builder/portal.md): a portal person whose portal roles reach some of its entities gets it as an entry of the portal's home, leading to Studio's generic page for that menu (`/portal/records/m/{name}`), with only the entities they reach. Nothing changes for members.
* `color`, `description`, `create`, `assistant` and `portal` are refused on a package's module in `package.yaml`: an app gives a tile, quick create, the assistant's notes and portal pages through its manifest.
* A menu travels in change sets and packs as any customization does; a pack requires the packages of the entities its menus name, and Studio when a menu is marked for the portal (the plan says so when Studio is missing).

**Service targets** ([BD-04](builder/decisions.md#bd-04-approvals-and-business-time-bf3)). A workspace says "this must happen within so much working time" of any entity whose records are saved on their own (not lines, not a ledger) with `add_target`; the platform keeps the due moment and a state word on each record and marks a breach once:

```json
{"entity": "Ticket", "op": {"op": "add_target", "target": {
  "name": "respond", "label": "First response", "calendar": "office",
  "start_when": "doc.status != 'Draft'",
  "within": "doc.priority == 'High' ? 'PT1H' : 'PT4H'",
  "pause_when": "doc.status == 'Waiting'",
  "met_when": "doc.status == 'Closed'",
  "due_field": "x_respond_by", "state_field": "x_respond_state"}}}
```

* `calendar` names a business calendar (above): time counts only in its open hours. Without one it is plain elapsed time. The calendar must be there when the target is saved, and deleting a calendar a target names is refused (409, naming the entity and the target).
* The expressions see `doc` as it will be stored (after defaults, fetched values, computed fields, Logic derivations and rules), `user`, `today` and `now`. `start_when` (optional) starts the clock at the first save on which it holds; without it the clock starts when the record is created. `within` gives seconds or an ISO 8601 duration (`PT8H`, `P2D`; at most a year); null or empty means the record has no such target. `pause_when` (optional) stops the clock while it holds. `met_when` meets the target.
* `due_field` is a date-time field and `state_field` a data or select field (a select offering `running`, `paused`, `met`, `met_late` and `breached`); neither is required, computed, rolled up or fetched, and no two targets share one. On an entity that comes with a package both are fields the workspace added (`x_…`). At most 8 targets on an entity.
* Both fields are the platform's to write, as a total is: what a person, an import, an effect or an API call sends for them is ignored, and an effect may not name them. `GET /api/meta/{Entity}` lists the entity's `targets` and marks each such field `kept_by_target: <name>`; the entity's JSON Schema marks it `readOnly`.
* On every save of a record (create, update, a configured action's effects, a workflow transition, submit, cancel), in the save's transaction, each target: starts a round when it has none running and `start_when` holds (unless `met_when` already holds); pauses while `pause_when` holds (the due moment is empty while stopped) and resumes when it stops holding, with the remaining time added from then; when `within` gives another duration while running, the due moment is the new remaining time from now (due now if it is used up); when `met_when` holds the round is `met`, or `met_late` if it was breached or its due moment had passed; a met round re-opened (`met_when` no longer holds) starts the next round; when `within` gives nothing a running round is dropped and both fields are emptied. An expression that cannot be evaluated refuses the save with a field error on the state field.
* A breach: once a minute the loop that runs scheduled automations marks rounds whose due moment has passed and that are not met, paused or breached. Each is marked once however many servers look, then its two fields are written as a workflow transition writes them, in any document status (a submitted document too) and past the record's rules (a breach is the platform's fact, not a person's edit), with a new version, an audit entry and the record's `updated` event: an automation on `updated` with `doc.x_respond_state == 'breached' && old.x_respond_state != 'breached'` reacts to it. If that fails (the calendar is gone, an expression no longer evaluates), the breach stays recorded and the field reads `breached` from the record's next save. A reminder before the due moment is a scheduled automation relative to the due field (`{"relative": {"field": "x_respond_by", "offset": "-PT1H"}}`), which follows the field when a pause or a new priority moves it.
* The same name again replaces a target where it stands (the earlier customization is marked `superseded_by`); `{"op": "remove_target", "name": "respond"}` removes one. A target on a custom entity is part of its definition: a later `redefine_entity` replaces it, and `GET /api/customize/definition/{Entity}` carries `targets`. Export and import and packs carry targets as customizations; a pack carries the calendars its targets name (below).
* A target added to an entity that has records starts on each record at its next save; nothing goes back over existing records. Each record keeps one round per target (`record_target`); a deleted record's rounds go with it.

A help desk end to end (a `Ticket` with `priority` and `status`, in Kolkata): the office's hours, the fields and the target, a reminder an hour before the due moment, and a mail on the breach.

```
PUT /api/admin/calendars/office
{"zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": ["2026-11-10"],
 "hours": {"mon": [["09:00", "13:00"], ["14:00", "18:00"]], "tue": [["09:00", "13:00"], ["14:00", "18:00"]], "wed": [["09:00", "13:00"], ["14:00", "18:00"]],
           "thu": [["09:00", "13:00"], ["14:00", "18:00"]], "fri": [["09:00", "13:00"], ["14:00", "18:00"]]}}

POST /api/customize/batch
{"ops": [{"entity": "Ticket", "op": {"op": "add_field", "field": {"name": "x_respond_by", "type": "datetime", "label": "Respond by"}}},
         {"entity": "Ticket", "op": {"op": "add_field", "field": {"name": "x_respond_state", "type": "data", "label": "Response"}}},
         {"entity": "Ticket", "op": {"op": "add_target", "target": {"name": "respond", "label": "First response", "calendar": "office",
            "within": "doc.priority == 'High' ? 'PT1H' : 'PT4H'", "pause_when": "doc.status == 'Waiting'", "met_when": "doc.status == 'Closed'",
            "due_field": "x_respond_by", "state_field": "x_respond_state"}}}]}

POST /api/admin/automations
{"name": "Response due soon", "entity": "Ticket", "event": "scheduled", "is_active": true,
 "schedule": {"relative": {"field": "x_respond_by", "offset": "-PT1H"}},
 "actions": [{"type": "notify", "to": "owner", "title": "'Respond within the hour: ' + doc.subject"}]}

POST /api/admin/automations
{"name": "Response breached", "entity": "Ticket", "event": "updated", "is_active": true,
 "condition": "doc.x_respond_state == 'breached' && old.x_respond_state != 'breached'",
 "actions": [{"type": "email", "to": "'desk-leads@example.com'", "subject": "'Breached: ' + doc.subject", "body": "doc.subject"}]}
```

**Published lists** ([BD-15](builder/decisions.md#bd-15-published-lists)). A workspace shows some fields of some records of an entity to anyone, without signing in: a register of issued permits, a price list, the classes on offer. It is a read-only allow-list:

```json
{"entity": "XPermit", "op": {"op": "publish_list", "list": {
  "name": "permits", "label": "Issued permits", "description": "Trade permits in force",
  "fields": ["number", "holder", "kind", "category", "issued_on"],
  "filter": [["kind", "=", "Trade"], ["expires_on", ">=", "2026-11-10"]],
  "sort": ["issued_on", "desc"], "csv": true, "limit": 500}}}
```

* `name`: lowercase letters, digits and hyphens, at most 60, one of a kind in the workspace (it is part of the public address); the same name again on the same entity replaces the list (the earlier customization is marked `superseded_by`), on another entity it is refused. `{"op": "unpublish_list", "name": "permits"}` takes it down. At most 20 lists a workspace, 1 to 12 fields a list, `limit` 1 to 1000 rows (200 without it).
* On a master or a document (not lines, a ledger or settings). A submittable entity's list shows submitted records only, whatever the filter says: a draft or a cancelled document is never public.
* `fields` is everything that is sent, in that order: the entity's own stored fields and `number`. Never the record's id, owner or timestamps. A listed link field shows the linked record's title (its title field, or its number), never its id.
* Refused when saved, each with why: a field of type email, phone, secret or attach; a table field; a field in a permission group (some of the workspace's own people may not read it); a field marked `no_export`; a field a package declares as people's personal data; any list of an entity whose records a package declares as people's personal data (a workspace's own entity is never declared, so there the administrator's choice stands with the rules above); a link to an entity that names itself only to those who may read it (`reference: restricted`), to one whose records are declared personal data, or whose title field could not be published itself; a field that is not there.
* `filter` (`[[field, op, value], …]`, the operators of list queries) and `sort` (`[field, "asc" | "desc"]`, newest first without it) may use any stored field of the entity, and `number`, `docstatus`, `workflow_state`, `created_at` and `updated_at`: they decide which rows show and in what order, and their values are not sent unless listed. A filter whose value does not fit its field is refused when saved.
* Later changes: a field that goes from the entity (its `add_field` switched off, a package upgrade), or may no longer be published, drops out of the list with a schema warning and the list stands (a customization that would do it, such as a `redefine_entity` without the field, is refused while the list names the field); a field added to the entity shows only once listed. A list whose filter or order names a field that went is taken down with a warning instead, as it would show more than was chosen. Switching the list's customization off takes it down.
* Served, read as the workspace, for an active workspace only:
  * `GET /api/public/{workspace}/lists/{name}` → `{"label", "description", "fields": [{"name", "label", "type"}], "rows": [{…the listed fields…}], "more": bool}`: the first `limit` rows, and whether there are more. There is no next page: a cursor would carry the last record's id and time, which the list does not show.
  * `GET /api/public/{workspace}/lists/{name}.csv` when `csv` is true: a header of the labels, then the rows (CRLF); every cell passes the platform's guard against spreadsheet formulas (`ferrum_kernel::published::csv_cell`: text beginning `=`, `+`, `-`, `@`, a tab or a carriage return gets a leading `'`, a number stays one; quotes doubled, quoted when it holds a comma, a quote or a line break).
  * `/public/{workspace}/lists/{name}`: a plain page with the workspace's name, the list's label and description, a table and a link to the CSV when offered. Every value is escaped; no script; `Content-Security-Policy: default-src 'none'; style-src 'sha256-…'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`; `noindex` (a meta tag and `X-Robots-Tag`).
  * No such workspace, an inactive one, no such list, one taken down, and `.csv` of a list without CSV all answer the same `404 {"error": "not_found", "message": "there is no such list"}` (the page: the same 404 page).
  * Every request counts against the public limiter (300 a minute per address and workspace, shared across replicas) before anything is looked up: past it, 429. An answer may be kept for a minute (`Cache-Control: public, max-age=60`); the query waits at most 5 seconds.
* `GET /api/meta/{Entity}` lists the entity's `published_lists` (`name`, `label`, `fields`, `csv`) to administrators only. Change sets and packs carry a list as any customization; a pack's plan says, as a warning to read before installing, `This pack publishes a list of Permits to anyone, without signing in (permits): Number, Holder, Type, Category, Issued on.`
* `lists` is not a plugin's name: plugins' public routes are `/api/public/{workspace}/{plugin}/…`, and `public` is not an app's route.

A low-priority ticket saved on Monday at 09:00 there reads `x_respond_by` 13:00 and `running`; put on `Waiting` at 11:00 it reads `paused` with no due moment; back on Wednesday at 09:00 (Tuesday is a holiday) it is due at 11:00, and the reminder fires at 10:00. Left alone, a minute after 11:00 it reads `breached` and the leads get one mail; closed after that, `met_late`.

**Lines within lines** (docs/builder/decisions.md BD-06, amended 11 November 2026). A custom line entity may hold a table of another custom line entity: a control's tests, each with its evidence rows. Only a workspace's own entities do this; a package's child entity holds no table. A record is written and read with its lines' lines nested in each line, in order, each row with its `id` and `idx` (its place, 0-based, at its own level), rollups and computed fields worked out at each level:

```json
{"code": "A-1", "total": "0.3", "tests": [
  {"id": "…", "idx": 0, "name": "door", "spent": "0.3", "pieces": 2, "evidence": [
    {"id": "…", "idx": 0, "title": "photo", "amount": "0.1", "tag": "door/photo"},
    {"id": "…", "idx": 1, "title": "log", "amount": "0.2", "tag": "door/log"}]}]}
```

A save replaces a table's rows as it does today, at either level: a row sent with its id keeps it, and what it held unless sent (a line sent as `{"id"}` alone keeps its own lines); a row without one is new; a row left out is deleted, a line's own lines with it. A line's line sent under another line than its own is refused (`this row is stored under another line: a row stays under its line (send it without its id to copy it)`); an id from another record makes a new row, as for lines. In a line's line, `parent` is its line (its rules, computed fields and defaults); a line's `rollup` totals its own lines and the record's rollup may total the lines' totals, both exact. An error in a line's line keeps `row` at its line and adds `path`: `{"field": "title", "message": "Title is required", "row": ["tests", 0], "path": [["tests", 0], ["evidence", 0]]}`. `GET /api/meta/{Entity}` nests `children` in a line that has tables of its own; `GET /api/customize/definition/{Entity}` lists the lines' lines' entities in `lines` too. A list filter reaches a record's lines, not their own lines (`cannot filter on 'tests.evidence.title': a filter reads a record's lines, not their own lines`); a line's line holds no unique key (no line does). A line entity whose rows exist cannot become another entity's lines (`Evidence: its rows are lines of XTest records; they cannot become the lines of XAudit.loose while there are any`). `append_line` and `set_line` write the record's own lines (the lines' own lines stay as they are), and `links.<field>` shows a linked record's lines with their own. The workspace's export, an import of it, a pack's reference records, merging and a link's delete guard carry or reach lines' lines too.

**Lines on a package's entity** (docs/builder/decisions.md BD-06, amended 10 November 2026). A customization adds a table of lines to a package's master, document or settings entity with `add_field`: the field is `x_…`, `type: table`, `of` a custom child entity (`define_entity` first) that is the lines of no other table and holds no table of its own. A total over them is another custom field (`rollup`). A checklist on a basket:

```json
{"entity": "Basket", "op": {"op": "add_field", "field": {"name": "x_checks", "type": "table", "of": "XCheck", "label": "Checks"}}}
{"entity": "Basket", "op": {"op": "add_field", "field": {"name": "x_score", "type": "currency", "rollup": {"table": "x_checks", "op": "sum", "field": "score"}}}}
```

A basket is then written and read with them like any lines, each row with its `id` and `idx`: `{"name": "B1", "x_score": "0.3", "items": [...], "x_checks": [{"id": "…", "idx": 0, "step": "weigh", "score": "0.1"}, {"id": "…", "idx": 1, "step": "seal", "score": "0.2"}]}`. Refused: on a package's line entity or a ledger (`table field x_more: BasketItem is the lines of another entity or a ledger, and takes no lines`), of a package's line entity (`the lines a workspace adds must be a custom child entity, and BasketItem is not one`), of a line entity with lines of its own (`… go one level deep, and XDeep has lines of its own`), and `add_field` of a table on a custom entity (its tables come with `define_entity`/`redefine_entity`). Deleting the package's record deletes its lines, also while the customization is off; switching it off hides them and keeps them; a package upgrade leaves them; uninstalling the package keeps them with its data, or deletes exactly them with it. List filters (`x_checks.step`), record scopes, the delete guard and merge's links, erasure's history redaction, the workspace's export and an import of it reach them as a custom entity's lines; a merge moves no lines, the package's or these. Another entity cannot take over their entity's rows while any are under a package's record (`Check: its rows are lines of another app's records; they cannot become the lines of XThief.rows while there are any`).

An authoring screen saves a whole design at once with `POST /api/customize/batch` `{"ops": [{entity, op, note?}, ...], "dry_run"?: bool}` (administrators; at most 100 operations, in a request body of at most 2 MiB like every JSON request). The operations are applied in order in one transaction, or none is: each is checked against the schema the ones before it leave, so a child entity comes before the entity whose lines it is. Success is `200 {"applied": n}`. A refusal has the status and body `POST /api/customize` would give, plus `index`, the 0-based position of the operation refused. An operation that does not parse (an unknown property, a missing one) is refused the same way, `422 {"error": "invalid", "index": n, "message": "unknown field `may_run`, expected …"}`; a body that is not JSON, or whose `ops` is missing or not a list, is `422 {"error": "invalid", "message"}` without an index:

```json
POST /api/customize/batch
{"ops": [{"entity": "XStep", "op": {"op": "define_entity", "entity": {"name": "XStep", "label": "Step", "kind": "child", "fields": [{"name": "what", "type": "data"}]}}},
         {"entity": "XTask", "op": {"op": "define_entity", "entity": {"name": "XTask", "label": "Task", "kind": "document", "fields": [{"name": "steps", "type": "table", "of": "XNothing"}]}}}]}
→ 422 {"error": "invalid", "index": 1, "message": "customization rejected: XTask: customizations ignored: table field 'steps' uses unknown entity XNothing"}
```

An operation exactly like an active customization is refused (409) rather than skipped, so "nothing changed" is not hidden. With `"dry_run": true` everything is done, unique keys filled and checked for records sharing a value included, and then undone: `200 {"ok": true}` or the refusal, with nothing saved and no new schema version.

`redefine_entity` replaces the definition an earlier `define_entity` or `redefine_entity` gave one of the workspace's own entities (not a package's: those take customizations one by one); switching it off (`PUT /api/customize/{id}/active`) returns the entity to the definition before it. Changes made with other operations before it are part of what it replaces; a workflow or print template set on the entity stays. The new definition passes the checks `define_entity` does, and these, while the entity has records:
- its kind (master, document, child) does not change;
- a field's type changes only so that its values still read: among data, text, markdown, code, email, phone and url; select to any of those; int to decimal, currency or percent; among decimal, currency and percent. Any other change (date to datetime, decimal to int, anything to json) is refused while a record holds a value in that field, naming the field;
- no data is deleted. A field that goes keeps its values in the records, out of sight, and a field of the same name that comes back sees them again; a table field that goes leaves its rows the same way. Unique keys that come are filled from the records (refused, listing them, where records share a value) and keys that go take their rows;
- a field that becomes required, and a new total over lines, apply from each record's next save.

A redefinition replaces every definition change before it, earlier redefinitions included: the list marks each of them `superseded_by` it, and switching one of those off changes nothing (the `define_entity` that made the entity is not marked: the redefinition needs it). So a designer starts from the definition as it is now, not from the latest definition row: `GET /api/customize/definition/{Entity}` (administrators) → `{"entity": EntityDef, "lines": {"XVisit": EntityDef, ...}}`, the custom entity's definition after every active customization, in the shape `redefine_entity` takes, with the definitions of the custom child entities its table fields hold. Sent back unchanged as `redefine_entity` (the lines first), it changes nothing. A packaged entity is refused (422): it has no definition a workspace can replace.

The same guards hold when a redefinition is switched off or on. A change set (`export`, `import`) carries redefinitions like any other operation and skips what is already active. A batch checks its operations in order, one at a time; an import is checked as a whole, once all its operations are applied, since an export's order need not pass step by step.

Packs ([BD-07](builder/decisions.md#bd-07-packs-bf9)): one JSON document carrying what an administrator configured in a workspace, installed in another (administrators only).
- `GET /api/admin/pack?records=EntityA,EntityB&name=&label=&version=` → a pack: `{format: "ferrum.pack/1", name, label, version, exported_at, requires: {packages: {package: version}}, customizations: [{entity, op, note?}], roles: [{name, description, portal, permissions: [PermissionDef]}], automations: [{name, entity, event, schedule?, condition?, actions, is_active}], studio: [{entity, name, record}], solutions: [ferrum.solution-pack/1], records: [{entity, key: [field], rows: [{...}]}], calendars?: [{name, zone, weekends, holidays, hours?}]}`. `calendars` holds the business calendars the workspace's service targets name (left out when there are none). `name` is a slug (default `workspace`), `version` a whole number (default 1). It holds the active customizations in stored order; the roles the workspace made (a role no package seeded, leaving out the roles of support, solutions, plugins and services) with their rules, and the rules it added to a packaged role (any rule its package's manifest does not seed, as the role's permissions API takes them); the automations administrators saved (one that notifies a person by id is refused: a pack carries no people); Studio's reports, charts, number cards, dashboards and spaces that no package shipped, by label, a dashboard's or space's references to the others by label; the running release of each solution as a solution pack; and the records of each entity named in `records`, matched by its first unique key or else its title field, links by the key of the record they reach. An entity whose records may not travel is refused, naming why: a package declares people's data in it, it or its lines keep an email, a phone number or a secret, it is submittable, or it has no key or title field; a link to an entity holding people's details is refused too. Derived, read-only, secret and file fields never travel; neither do ids, users or other records. `requires` names the packages whose entities the pack names, at the versions installed. Two exports of an unchanged workspace differ only in `exported_at`.
- `POST /api/admin/pack/plan` `{pack}` → `{name, label, version, digest, requires, missing, can_install, sections?, warnings?}`: `missing` says what the workspace lacks (a required package not installed, or installed at an older version, or a business calendar a target names that is neither here nor in the pack); with nothing missing, the install is done in a transaction that is rolled back, and `sections` says per section (`calendars`, `customizations`, `roles`, `automations`, `studio`, `records`, `solutions`) the names it would create, change or leave alone (`{created, changed, unchanged}`); `warnings` says that a solution arrives as a draft a second administrator reviews and someone activates, and that a scheduled automation starts from now, and names each of the workspace's own customizations the pack would leave replaced that it does not carry itself ("XMembership: add_rule end_after_start stops applying: the pack redefines XMembership"). `digest` is the platform's canonical digest of the pack, leaving out `exported_at`.
- `POST /api/admin/pack/install` `{pack, digest}` → `{id, name, version, digest, summary, failed, warnings}` (`warnings`: the customizations of the workspace it left replaced, as the plan names them). Refused (422 on `digest`) unless `digest` is the pack's, so the plan an administrator saw is the one applied; refused (409) when something is missing. Everything but the solutions is applied in one transaction, through what the admin API uses, so every check applies and a refusal anywhere leaves nothing: calendars (one of the same name here is left as it is), customizations as an import (validated as a whole, exact duplicates skipped, a custom entity this workspace defines otherwise redefined under `redefine_entity`'s guards), roles and their rules (a portal role marked before its rules), automations (matched by name, saved as the installer), Studio's items (matched by entity and label, references found by label here), records (matched by key, created or updated, never deleted). Then each solution is taken in as a draft (`POST /api/app/studio/solutions/import/pack`'s rules: it runs only once kept as a release, reviewed by a second administrator and activated); one that fails is listed in `failed` with why, and installing again is safe. Installing the same pack again changes nothing. Each install is kept (`pack_install`) with what it did and audited (`Pack` `install`).
- `GET /api/admin/packs` → `{packs: [{id, name, version, digest, installed_by, installed_at, summary}]}`, the latest first.
- `GET /api/admin/capabilities` (administrators) → `{capabilities: [{id, title, entity, effect, needs_key, inputs: [{name, type, required, description}]}]}`: the workspace's capability catalog as a button's or automation's `invoke` names it, from the same kernel function Studio's catalog reads, without Studio. `needs_key` is whether it changes something (and so runs once per effect key).

A pack is read strictly: at most 5 MiB, and a key it does not know (at any level) is refused (422 on `pack`, naming it). A pack carries no code, SQL, credential, secret, user, file or record other than the reference rows named. Not yet: uninstalling a pack, signing one, an app store of packs, packs inside app bundles.

Plugins (sandboxed WebAssembly, see README): `GET /api/admin/plugins` → `{plugins: [{id, name, version, description, sha256, hooks, fuel_limit, memory_limit_mb, enabled, updated_at}]}`;
`POST /api/admin/plugins` `{name, version?, description?, hooks: [{entity|"*", stage: before_validate|validate|before_submit|on_submit|before_cancel|on_cancel|before_delete}], fuel_limit?, memory_limit_mb?, wasm_base64}` → `{id}` (same name replaces);
`PUT /api/admin/plugins/{id}/enabled` `{enabled}`; `DELETE /api/admin/plugins/{id}`.

`PUT /api/admin/plugins/{id}/config` `{config}` replaces a plugin's settings and secrets; `PATCH` merges the given keys. Plugin endpoints:
any method on `/api/ext/{plugin}/{path}` (signed in) or `/api/public/{tenant}/{plugin}/{path}` (public routes),
and WebSockets at `/ws/ext/{tenant}/{plugin}/{path}` (token in `Authorization` or `access_token` unless public).
Manifest, contract and host calls: [plugins.md](plugins.md).

People leaving and provisioning ([identity lifecycle](identity-lifecycle.md)): `POST
/api/admin/users/{id}/deprovision` `{reason, reassign_to?}` → `{user, sessions_ended, api_keys_revoked,
oauth_tokens_revoked, assignments_moved, assignments_left_open, tasks_handed_on, reassign_to}`; `GET
/api/admin/scim/groups` → `{groups: [{id, display_name, roles, members}]}`; `PUT
/api/admin/scim/groups/{id}/roles` `{roles}`.

Why someone may or may not do something: `GET
/api/admin/access/explain?user=&entity=&action=&record=&roles=` → `{allowed, reason, roles, rules:
[{role, entity, if_owner, where}], scoped_by, simulated, record?: {id, reached}}`. `action` is read,
write, create, delete, submit, cancel, export or import; `record` asks about one record; `roles`
(comma-separated) answers for those roles instead of the person's own. Nothing changes by asking.

Backfills (data a release moves in batches, [operations](operations.md#upgrades)): `GET /api/admin/backfills` →
`{backfills: [{package, name, state (waiting|running|done), changed, remaining, started_at, finished_at, needing_now}]}`;
`POST /api/admin/backfills/{package}/{name}/rerun` runs a finished one again at the next upkeep (409 while it runs).

Roles one person may not hold together (segregation of duties): `GET /api/admin/role-conflicts` →
`{conflicts: [{role_a, role_b, reason, created_at, holders: [{id, email, full_name}]}]}`, where
`holders` are those who held both before the pair was declared; `PUT` `{role_a, role_b, reason}`
declares a pair (or rewords its reason) and `DELETE ?role_a=&role_b=` allows the two together again.
From the moment a pair is declared the database refuses any grant that would join it, whichever way
the grant comes (the users API, SCIM groups, apps), with `422` on `roles` saying why.

Personal data, beyond the entry above: answers also carry `addresses` (every address the person has
had), `held`, `not_searched` (installed packages that declare nothing) and, per record,
`retained_because` and `files`. Erasure answers `{erased, retained: [{entity, id, because}], held,
files_deleted, audit_events_redacted, account_erased, not_searched}`. Legal holds:
`/api/admin/legal-holds` (GET → `{holds}`; POST `{email, reason}` or `{entity, record_id, reason}` →
`{id}`), `POST /api/admin/legal-holds/{id}/release`. Erasure and deletion answer 409 under one.

Failed events ([events](events.md)): `GET /api/admin/events/dead?limit=` → `{events: [{id, topic,
entity, record_id, attempts, last_error, created_at, correlation_id}]}`; `POST
/api/admin/events/{id}/replay`; `GET /api/admin/webhook-deliveries/dead?limit=` → `{deliveries: [{id,
webhook, webhook_name, topic, attempts, response_code, last_error, created_at, updated_at}]}`; `POST
/api/admin/webhook-deliveries/{id}/replay`.

Business calendars ([processes](logic/processes.md)): `GET /api/admin/calendars` → `{calendars: [{name,
zone, weekends, holidays, hours, revision, updated_by, updated_at}]}`; `GET /api/admin/calendars/{name}` →
one of them; `PUT /api/admin/calendars/{name}` `{zone, weekends, holidays, hours?}` → `{revision}`, each
change a new revision; `DELETE /api/admin/calendars/{name}`. `hours` is windows per weekday in the zone,
`{"mon": [["09:00", "13:00"], ["14:00", "18:00"]]}`; left out, every hour of a business day is open. Bad
hours are a field error on `hours`. These are the functions Studio's `/calendars` route calls. Deleting a
calendar a release's waits name is not refused: deadlines already worked out stay, and a wait that starts
later on it fails as one naming a calendar the workspace never had.

Personal data, beside `/api/admin/privacy`: `GET`/`POST /api/admin/restrictions`, `POST
/api/admin/restrictions/lift` `{email}`; `GET`/`POST /api/admin/retention`, `PUT`/`DELETE
/api/admin/retention/{id}`, `POST /api/admin/retention/{id}/preview` → `{reaches, more}`.

Services Drive and printing depend on (administrators): `GET /api/admin/services` → `{services: [{check, level, detail, fix?}]}`,
the online editor (`office`: set up, answering, offering docx, xlsx and pptx; `office.proof`: signing its WOPI calls) and
the PDF renderer (`pdf`), each `ok`, `warning` or `blocker`; no address or token is in it, so it can be shown as it is.

Customizations and upgrades: `GET /api/customize/conflicts` → `{conflicts: [{id, entity, op, was,
now, gone, suspended}]}`; `POST /api/customize/{id}/keep` keeps one over what its package says now.
A customization keeps what it replaced (`was`) when it is made or kept: a field's or the entity's
property, a whole workflow (`set_workflow`) or print template (`set_print_template`), `null` where
the packages had none. One made before it kept that is compared from what its packages say when this
version first starts. An app's plan says the same before anyone agrees to it (below).

## SCIM 2.0 — `/api/scim/v2` ([identity lifecycle](identity-lifecycle.md))

For identity providers, with an API key of scope `scim`, which opens nothing else: `Users` and
`Groups` (GET, POST; `/{id}` GET, PUT, PATCH, DELETE), `ServiceProviderConfig`, `ResourceTypes`,
`Schemas`. Bodies and answers are `application/scim+json`, and errors are RFC 7644's, so what the rest
of this API answers with 422 is 400 here, with a `scimType`.

## Apps — `/api/admin/apps` (administrators; [design](plan/apps.md))

`AppSummary` = `{id, name, version, runtime: native|wasm|meta, summary, categories[], publisher?, verified, icon_url, source: native|upload|store:<url>, private}`.

* `GET /api/admin/apps` → `{installed: [AppSummary + {enabled, installed_at, update: {version}|null}], available: [AppSummary], uploads: off|private|cluster, platform, stores: [{url, ok, error}]}`.
  `installed` also lists native apps whose packages the workspace already has (installed when it was created); `available` holds this build's native apps, releases available on the cluster and the workspace's private uploads. `update` is a newer compatible release on the cluster (from the stores too when `uploads` is `cluster`), or this build's version of a native app.
* `GET /api/admin/apps/store?q=&category=` → `{apps: [AppSummary]}`: matching apps available here first, then the stores' results (cached one minute; a store that does not answer is skipped).
* `GET /api/admin/apps/{id}/plan?version=` → `{app: AppSummary + {description, license, homepage}, steps: [{id, name, version, action: install|upgrade|already, source: native|cluster|private|store:<url>}], permissions, digest, increases[], warnings[], uninstall: keep|ask, customizations[], open_work[]}`.
  `customizations` are the workspace's customizations whose packages the plan changes underneath, as `GET /api/customize/conflicts` will list them once it is carried out (one that parted ways earlier, and is not moved again, is not the plan's); `open_work` is `[{entity, state, records, removed}]`, the records waiting in a workflow state the plan removes (`removed`) or whose ways out, or what it lets people do, it changes: they go on under the new definition. Both are worked out with the packages the plan brings in place of the cluster's, and each has a line in `warnings`. A release is the cluster's, so every other workspace with its packages takes it as it is made available: their administrators are told what it changed there (a notification, "{app} {version} changed this workspace").
  `description` is the opening prose of the app's description file (its README) as plain text, about 600 characters at most, without its HTML, pictures, badges, headings, tables or code (`ferrum_apps::description::excerpt`).
  Steps are in installation order (dependencies first). `permissions` covers everything the plan installs or upgrades (the shape of `ferrum_apps::Permissions`); `increases` lists what upgrades ask for beyond the installed versions; `digest` = SHA-256 over the ordered steps' `(id, version, permissions digest)`, then each customization the plan parts, `(id, was, now)`: a customization made or switched on after the plan was reviewed changes it. Store bundles are downloaded and verified while planning.
* `POST /api/admin/apps/{id}/install` and `/upgrade` `{version, consent}` (`consent` = the plan's `digest`) → the plan carried out. The plan is computed again: a different digest answers `409 {"error": "conflict", "message", "plan": <fresh plan>}`. Store releases are made available on the cluster (verified: a trusted store countersignature, the listed digest, the role rule), then each app's packages, plugin and record are installed; a failure takes back what the call did in the workspace. `upgrade` needs the app installed; `install` on an installed app applies its plan as well (as after uploading a newer version).
* `POST /api/admin/apps/{id}/disable` / `enable` → 204. Disabling turns the plugin off and takes the app's packages out of the workspace (rows kept; role assignments to the app's roles are dropped); refused while an enabled app requires it. Enabling is refused for a revoked release.
* `DELETE /api/admin/apps/{id}?delete_data=true` → `{removed, deleted_data, leftovers}`: packages out (with their roles), plugin and its private data removed, record forgotten. The app's data is deleted only with `delete_data` **and** `uninstall: ask` in its manifest (`keep` apps never lose data): the workspace's rows in its entities' tables and in the tables its packages' SQL files create, the files attached to its records (their bytes leave storage once the removal commits) and its live documents. Refused while another installed app requires it. `leftovers` are the apps its installation brought in (a suite's members, what it required) that nothing installed needs now, in an order they can be removed in; an app an administrator installed for itself is never among them.
* `GET|PUT|DELETE /api/admin/apps/{id}/dev-ui` (development servers only, `FERRUM_DEV_UI=true`; `404` elsewhere) → `{app, dev_ui}`: `PUT {url}` serves the installed app's pages in this workspace from its interface's Vite server on this machine (`http://localhost:<port>`, `127.0.0.1` or `[::1]`), behind the real session, bar and bootstrap, with hot reload (`ferrum-app dev --ui`); an isolated interface stays isolated, framed from there. The pages' release is `dev` (no "new version" notices), and their policy allows that server's files and socket. `DELETE` serves the release again; a restart forgets them all.
* `POST /api/admin/apps/upload` (multipart, field `bundle`, ≤ 50 MB) → `201 {app: AppSummary, plan}`. Verified against `apps.trusted_keys`; unsigned or untrusted bundles only with `apps.allow_unsigned_uploads`. `uploads: private` accepts sandboxed apps without packages, visible only in this workspace; `cluster` makes the app available to every workspace. Native apps, apps for another platform version, older versions than the one available, and roles granting entities the app does not define (the role rule) are refused.

* `PUT /api/admin/apps/{id}/licence {licence}`: the licence of an app a store sells (bought there for this workspace), kept once a store this server uses signs it, it is for that app and for one of the workspace's own hosts, and it is live; `DELETE` forgets it; `GET /api/admin/apps/licences` → `{"licences": [{app, store, live, period_end, checked_at, last_error}]}`. A sold app installs or upgrades only with a live licence, asked of its store at that moment (`403` otherwise, saying why); every licence is read again with the revocation list, and a lapse shows there without removing anything.

### App stores (what this server reads)

Stores are configured in `apps.registries`. Only an app's `id` and a release's `version` are required; other fields are optional (alternative names in brackets are accepted).

* `GET {store}/v1/apps?q=&category=&platform=<PLATFORM_VERSION>` → `{apps: [...]}` (or `items`, `results`, `data`, or a bare array) of `{id, name, version [latest_version, latest.version], runtime, summary [description], categories, publisher [publisher.id], verified [publisher_verified, publisher.verified], icon_url [icon] (absolute, or relative to the store)}`.
* `GET {store}/v1/apps/{id}` → the same object, optionally wrapped as `{app: {...}}` (used for icons).
* `GET {store}/v1/apps/{id}/releases` → `{releases: [...]}` of `{version, requires {app: semver requirement} [manifest.requires], platform [manifest.platform], digest [sha256], bundle_url [download_url], status, yanked}`; 404 means the store does not have the app. Releases marked `yanked: true` or with status `yanked`, `revoked`, `rejected`, `pending` or `in_review` are ignored. A bundle's own `app.yaml` wins over the listing (requirements and platform are re-checked after download).
* `GET {store}/v1/apps/{id}/releases/{version}/bundle` (unless `bundle_url` says otherwise) → the `.fapp`, redirects followed. It must carry a store signature by a key in `apps.trusted_keys`, and match the listed `digest` when there is one.
* `GET {store}/v1/apps/{id}/plans` → `{"plans": [...]}`: an app with plans is sold, and installs only with a licence. A store without this answer sells nothing.
* `GET {store}/v1/licenses/{licence}` → a signed document as below, with payload `{"license", "app", "publisher", "plan", "workspace": "<host name>", "status", "live": bool, "period_end"}`.
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

The assistant (`/api/assistant`) adds to the context of each message what the app of the page it was
written in says about its records (`shell.assistant: [{id, title, instructions, suggestions}]`), and
`GET /api/assistant/suggestions?path=` gives that app's suggested questions for a new conversation.
Under each answer the panel lists the records, lists and reports its reads looked at.

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
* On the pages of a workspace's own menu (`/data/{menu}/…`) the menu's `assistant.instructions` are
  given with the page as an app's `shell.assistant` are, and `GET /api/assistant/suggestions?path=`
  offers its `suggestions` there.
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

`POST /api/files` (multipart: `file`, optional `entity`, `record_id`, `public`; ≤25 MB) → `{id, url: "/files/<id>", file_name, size, sha256}`; `GET /files/{id}` (authenticated download, streamed; one `Range: bytes=…` answered with 206).

Who may attach and download (the uploads below alike):

* Tying a file to a record (`entity` and `record_id`) needs the right to change that record (write permission, and its scope admits the record), for members and portal people alike: reading it is not enough (`403`). Comments carry no files, so there is no narrower right for them.
* A file tied to a record is downloaded by whoever may read the record. A file tied to none (an upload for an `attach` field of a record not saved yet) is downloaded only by whoever uploaded it and administrators (`404` for anyone else).
* Saving a record whose `attach` field (its child tables' included) newly names `/files/<id>` ties a file tied to none to that record when the saver uploaded it (or is an administrator); a file already tied to a record stays there and may be named by someone who may read that record. Naming any other file is refused (`403`), so an id is no way to read a file.
* Portal people and guests upload at most 20 files tied to no record an hour (`429` past it); sizes are limited as for everyone.

Larger files (up to 2 GB by default, S3-compatible storage only) go straight from the client to the bucket:

1. `POST /api/files/uploads` `{file_name, content_type, size, entity?, record_id?}` → `201 {upload_id, method: "PUT", url, headers, expires_at}`. `409` when the server stores files on a local disk.
2. Send the bytes with that method, URL and headers (valid for an hour).
3. `POST /api/files/uploads/{upload_id}/complete` → `201` and the same body as `POST /api/files`. The server copies the object to the file's own key (later requests to the upload URL cannot change it), checks its size against `size` (a mismatch discards it) and records its SHA-256. `409` if nothing has arrived yet or the upload expired.

Or, on any store, through the server in parts that survive a dropped connection (up to the same 2 GB):

1. `POST /api/files/uploads` `{file_name, content_type, size, entity?, record_id?, parts: true}` → `201 {upload_id, parts: true, part_bytes, size, offset: 0, expires_at}`. Refused at once when the workspace has no room for `size`.
2. `PUT /api/files/uploads/{upload_id}/parts/{offset}` with the next bytes as the body (1 to `part_bytes`, 8 MiB) → `{upload_id, size, offset}`, the offset the next part starts at. A part must start where the upload stands: `409` otherwise (one sent again after its answer was lost, say). Each part keeps the upload for another 24 hours.
3. After a dropped connection, `GET /api/files/uploads/{upload_id}` → `{upload_id, parts, size, offset, part_bytes, expires_at}` says where to go on from.
4. `POST /api/files/uploads/{upload_id}/complete` once every byte has arrived (`409` before) joins the parts and records the file as above. The parts stay until it is recorded, so a completion that failed (the scanner down) can be asked for again.

5. `DELETE /api/files/uploads/{upload_id}` cancels it: the upload is forgotten and its parts deleted at once → `204`. Only the uploader's own upload; cancelling again, or an upload that is not there (completed, swept, someone else's), changes nothing and answers `204` too. A part already being stored when the cancel comes lands first and goes with the rest; a part sent after it, and a completion, find no upload (`404`): a cancelled upload cannot come back. Nothing of the workspace's room is held for an upload (it is checked when the upload starts and again when it completes), so there is nothing to give back. An upload straight to object storage cannot be cancelled (`409`): its URL takes bytes until it expires, an hour after it started.

Uploads never completed are deleted with their bytes after they expire: every ten minutes each replica deletes up to 500 that expired more than five minutes before, with their parts or staged object. `GET /api/version` → `uploads: {max_bytes, direct, direct_max_bytes, parts, parts_max_bytes, part_bytes}` tells clients which way a file goes.
Public files (`public=true`; images and PDFs only — logos, product photos, brochures) are served without signing in at `GET /files/public/{id}` with an immutable cache and a sandboxing CSP. Files live in S3-compatible storage in production.

## System

`GET /api/ready` (DB reachable), `GET /api/version`, `GET /metrics` (Prometheus), `POST /api/tenants` (requires `X-Ferrum-Bootstrap-Token`; a slug any workspace ever used is taken for good), `GET /_ping`, `GET /_health`.

## ERPNext compatibility

`GET|POST /api/resource/{DocType}`, `GET|PUT|DELETE /api/resource/{DocType}/{name}` (`name` = document number or id; DocType names map by removing spaces), `GET /api/method/frappe.auth.get_logged_user`. Responses use `{"data": ...}`.
