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
| `POST /code/sign-up` | public sign-up to a portal role (below, "Public sign-up"): `{tenant, role, name, email, passes?}` → `{"sent": true}`; the code it mails is confirmed at `/code/sign-in` |
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
  `{name, label, label_plural, kind (master|document|child|settings|ledger), module, title_field, submittable, mergeable, linked_from: [{entity, label, field, field_label}] (the lists the caller may read that link here; `field` is a filter path, so `items.item` for a child table's row), company_scoped, numbering, list: {columns, sort}, form: [{label, fields, columns (1-4), collapsible, tab?, visible_if?}], search_fields, rules, actions: [{name, label, docstatus[], permission, confirm?, condition?, read_only?, params?, group?, roles?, refuse_if?, effects? (administrators only), may_run}], workflow?: {initial_state, states: [{name, docstatus, edit_roles, editable? (the only fields a person changes in the state), required? (what it needs to be entered and saved)}], transitions: [{action, from, to, roles, condition?, not_creator, not_creator_of? (links, `field` or `table.field`, whose records the person taking it may not have raised), params? (what it asks for, as an action's), effects? (what it does, as a configured action's), approval? (who must agree first, [Approvals](#approvals-on-a-transition)), may_take (whether the caller may take it, as taking it asks)}]}, fields: [Field], children: {tableField: {entity, label, fields: [Field]}}, permissions: {read|write|create|delete|submit|cancel|export|import: "none"|"own"|"all"}}`
  An action's `may_run` says whether the caller may press it at all: its permission or a role's rule naming it, and its `roles`, as running it checks them (an administrator always may); `condition`, `docstatus` and `refuse_if` are judged per record, so a form evaluates them on the record it shows. A configured action's `effects` (what it writes) go to administrators only.
* `Field`: `{name, label, type, required?, unique?, read_only?, hidden?, in_list?, in_filter?, options?, to?, of?, precision?, currency?, min?, max?, max_length?, default?, default_expr?, computed?, rollup?, visible_if?, required_if?, read_only_if?, read_only_message?, fetch_from?, description?, widget?, link_filters?, storage: "column"|"ext", can_read, can_write}`.
  `rollup` (`{table, op: sum|count, field?}`) is an exact total over the record's own rows of `table`: the sum of a number field of the rows, or their count. The server computes it at every save and in the draft preview, before validation, in exact decimals; a value sent for it is ignored.
  `link_filters` (`{field: value}` on the linked entity) limits a link's choices, checked on write: a literal, or `{"from": "<field>"}` (equal to that field of the record being saved) or, on a line's field, `{"from": "parent.<field>"}` ([BD-52](builder/decisions.md#bd-52-a-link-limited-by-another-field-of-the-record-and-the-app-api-036)). An empty source field applies no filter; a mismatch is refused on the field ("Store Yard belongs to Company B; this record is for Company A"). A picker resolves `from` against the form and sends it as an ordinary equality filter on the target's list: `filters={"company": "<id>"}`.
  Types: `data text markdown code int decimal currency percent date datetime check select link table json email phone url attach color secret`.
  `visible_if`/`required_if`/`read_only_if` are CEL over `doc` (and `parent` in child rows); the server is the authority. `read_only_if` is judged on the record (or row) as stored before the change: while it holds, the field ignores input exactly as a `read_only` field does for that caller (the system and an amendment's copy still write it); a new record or row has nothing stored, so only `read_only` counts for it, and a condition that cannot be evaluated counts as holding. A field may say why instead: with `read_only_message` ("went to the kitchen: void it and order again"), a person's value that differs from the stored one while `read_only_if` holds refuses the save with 422 and that message on the field (and the row, for a line); both values are compared after normalising, so a form sending the whole record back unchanged passes. The system, an amendment's copy and a field that is `read_only` outright still ignore the input silently; table and secret fields do not take the message.
  A link field to its own entity makes a hierarchy: a record cannot be its own parent, and a move that would make a loop is refused on the field naming both records (`Asia cannot sit under India: India is already under Asia, which would make a cycle in the hierarchy`).
  `widget` says how forms edit a field where its type alone does not: `studio_query` on a `json` field holding a Studio query, which forms build with Studio's report builder and show the first rows of (the field's own rules still decide what it may hold).
  CEL also sees `user` and `today`, `now`, `time` (local to the workspace). A record's whole numbers are signed integers, as literals are (`doc.days * 24`, `doc.owed > -100`); decimals are floats for comparison. Functions for dates, text and empty values (`addMonths`, `formatDate`, `lower`, `padStart`, `isEmpty`, `coalesce`...) are listed in [docs/builder/expressions.md](builder/expressions.md). Computed fields are worked out each after the computed fields it reads (`doc.<field>`), by name where nothing decides; one reading itself reads its stored value, and computed fields reading each other in a circle are refused when saved.
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
| `POST /api/v1/{E}/{id}/workflow/{action}` | workflow transition. One naming `roles` is taken by a holder of one of them on a record they can read; one naming none by whoever may change the record (write permission, and its scope admits the record); an outside person (portal, guest) takes only one naming a role of theirs. `403` otherwise. Bulk, MCP, capabilities (`…transition.<action>`) and processes ask the same. The body, when there is one, is what the transition asks for (`{"reason": "…"}`): a required one missing or empty, or one it does not ask for, is a `422`. Its rules `on: transition` run, then the move, then its `effects` (as the system on the taker's behalf, in the same transaction; a refusal undoes all of it), and a move within one document status raises `<Entity>.transitioned`. With an approval policy the record stays where it is and the answer carries `_approval` (the request; [Approvals](#approvals-on-a-transition)) |
| `GET /api/v1/{E}/{id}/approval` | the record's latest approval request, or `null`: `{id, entity, record, action, from, to, requester, requested_at, status: pending\|approved\|rejected\|recalled\|cancelled, params, steps: [{label, state: waiting\|open\|skipped\|approved\|rejected, need, approvers: [user id], escalated: [user id], opened_at, remind_at, escalate_at}], decisions: [{step (0-based), decider, on_behalf_of, outcome: approve\|reject, comment, at, void}]}`. For whoever may read the record, its requester and its approvers (and whoever stands in for one) |
| `POST /api/v1/{E}/{id}/approval/decide` | `{outcome: "approve"\|"reject", comment?}` (a reject needs a comment) → the request as it now stands. By an approver of an open step, or whoever stands in for one today. The last approval completes the transition. `403` not yours to decide (or no longer, or not independent), `409` already decided, `404` nothing pending |
| `POST /api/v1/{E}/{id}/approval/recall` | the requester (or an administrator) takes a pending request back → the request |
| `POST /api/v1/{E}/_calc` | preview: runs defaults, fetches and calculation hooks on an unsaved body (include `id` to start from a saved record) → computed document, nothing written. Use for live totals |
| `GET /api/v1/{E}/{id}/print` | HTML print format; `?format=pdf` → the same as a PDF (`application/pdf`; `503` when the server has no PDF renderer, see `GET /api/version` → `pdf`) |
| `POST /api/v1/{E}/{id}/email` | `{to, subject?, message?, message_html?, pdf?: true}` → `202 {queued, to, subject, attachments}`: emails the document through the server's mailer, its print format attached as a PDF (stored on the record), and notes it on the record's timeline. `message` is text, escaped and its line breaks kept; `message_html` (at most 512 KB) is rich text from the interface's editor, cleaned by the kernel's allow-list (`crates/kernel/src/richtext.rs`: paragraphs, headings, quote, code, the inline marks, lists, links to http, https and mailto, https images, tables, rules; `dir` and a validated `style` of colour, size, family, alignment and width; no classes, ids, data attributes, handlers, relative or `data:` addresses) and taken over `message` when anything is left. Every mail the server sends goes as `multipart/alternative`, its text part made from the HTML (`richtext::html_to_text`). A server that takes `message_html` says `mail_html: true` in `GET /api/version`. Queuing the mail raises `<Entity>.emailed` on the outbox in the same transaction (docs/events.md), so webhooks and apps' event sinks hear of each mail that goes: the event's `actor` is who sent it, its payload `{entity, id, event: "emailed", actor, request_id, to, subject, pdf, attachments}` (`pdf` whether the PDF went along; `attachments` the stored files). Automations do not take it. Needs export permission on the entity; 30 per user per minute. `503` without a mailer, or for a PDF when the server has no PDF renderer or its mailer cannot attach (`GET /api/version` → `mail`, `mail_attachments`, `pdf`) |
| `GET /api/v1/{E}/{id}/audit` | `{events: [{at, actor, actor_name, action, version, diff}], chain_valid}` |
| `GET /api/v1/{E}/_export` | CSV (export permission), streamed a page at a time with every row however many; or Excel with `format=xlsx` (field labels as headers, numbers as numbers), at most 100,000 rows and refused (422) past them rather than cut short; same filters as list |
| `POST /api/v1/{E}/_import` | JSON array (≤5000) → `{created, ids, errors: [{row, error, fields?}]}` (`207` if some failed) |
| `POST /api/v1/{E}/_bulk` | one operation on a selection: `{ids: [...]}` or `{filters: [...]}` (≤500 records) plus `{"op": "update", "values": {...}}`, `{"op": "delete"}`, `{"op": "submit"}`, `{"op": "cancel"}`, `{"op": "workflow", "action"}`, `{"op": "action", "action", "params"?}`, `{"op": "assign", "user", "note"?, "due_date"?, "priority"?}`, `{"op": "add_tags" \| "remove_tags", "tags"}` → `{succeeded, failed, results: [{id, ok, error?}]}`. Each record runs in its own transaction with the caller's permissions; `error` has the API error shape |
| `POST /api/v1/{E}/_bulk/jobs` | the same over up to 100,000 records, or all a filter matches when it starts → `202 {job}`; see [Background jobs](#background-jobs--apijobs) |
| `GET /api/v1/{E}/_duplicates?limit=` | a mergeable entity's likely duplicates: `{groups: [{field, value, ids}]}` (needs delete) |
| `POST /api/v1/{E}/{id}/merge` | `{duplicate, reason, values?, version?}`: moves everything linked to the duplicate onto this record → `{keep, merged, links_moved}` (needs delete). Whole or not at all, and not undone. The old id then answers with this record. `values` are what was chosen from the duplicate to stay, written onto this record in the merge's transaction (at `version`). The duplicate's own lines stay with it: to keep a contact's second email, send this record's `emails` with that one added in `values`. Mergeable among the shared packages: Customer, Supplier and Contact (likely duplicates by `email`, `phone`, `full_name`). |

## Approvals on a transition

A workflow transition may carry `approval` (docs/builder/decisions.md BD-19): taking it then makes a request, and the
record stays in its state until the policy is satisfied; the transition then completes once, as the system on the
requester's behalf, with its effects and events, in the transaction of the decision that satisfied it. A transition
with `params`, `effects` and a policy:

```json
{"action": "Submit", "from": "Ready", "to": "Approved",
 "params": [{"name": "reason", "label": "Reason", "type": "text", "required": true}],
 "effects": [{"type": "set_fields", "values": {"note": "'approved: ' + params.reason"}}],
 "approval": {
   "steps": [
     {"label": "Manager", "approvers": {"field": "manager"}},
     {"label": "Finance", "approvers": {"role": "Finance"}, "need": 2, "when": "doc.amount > 10000",
      "with_next": false, "not_earlier_deciders": true, "not_who_took": ["Prepare"],
      "calendar": "Office", "remind_after": "PT8H", "escalate_after": "P2D", "escalate_to": {"role": "Director"}}],
   "allow_requester": false, "reset_on": ["amount"], "on_reject": "Rejected"}}
```

* **Steps** run in order; one marked `with_next` opens with the next, and both are satisfied before later ones open.
  A step whose `when` (over `doc` and `settings`) is false is skipped; when every step is skipped the transition
  goes through at once. At most 10 steps.
* **Approvers** name exactly one of `role` (its holders), `field` (a field of the record holding a user's id or e-mail
  address, or a list of them) or `expr` (an expression over `doc` and `settings` giving the same). A fixed person is
  an `expr` giving their id. Who may decide is worked out when the step opens, and checked again at each decision.
* **`need`**: a number, `all` (every approver the step opened with) or `any` (the default, one). The same person twice
  counts once. A step that opens with fewer approvers than it needs is refused, and so is what opened it.
* **Independence**: the requester never decides unless `allow_requester`; `not_earlier_deciders` keeps out anyone who
  decided an earlier step; `not_who_took` keeps out whoever took those transitions on this record.
* **Changes while pending**: a change to a field in `reset_on` cancels the decisions so far (kept, marked `void`) and
  opens the steps again; other edits leave them. Any other transition taken on the record ends the request.
* **Rejection** needs a comment; the record stays, or moves to `on_reject` (a state of the same document status).
* **Stand-ins**: someone away (`POST /api/away` `{starts_on, ends_on, substitute (an id or an e-mail), reason, user?}`, also at `POST /api/app/studio/away`, as for process tasks) has their decisions go to their
  substitute, who decides in their own name on their behalf; both are kept. One hop only; a substitute who is the
  requester may not.
* **Reminders and escalation**: `remind_after` and `escalate_after` (seconds or ISO 8601, working time on `calendar`
  when given) after a step opens, the minute loop reminds those who have not decided, and then lets `escalate_to`
  decide the step too, telling them; each once.
* **Notices** go to approvers (and their stand-ins) when a step opens and to the requester when it is decided, as the
  notification kind `approval`, by the channel each person chose.
* **Seeing it**: `GET /api/v1/{E}/{id}/approval`, the record's history (`approval:requested`, `approval:approve`,
  `approval:reject`, `approval:reset`, `approval:recalled`, `approval:reminded`, `approval:escalated`), and
  `GET /api/approvals` → `{to_decide: [Item], awaiting_others: [Item]}` with Item `{request, entity, entity_label,
  record, title, action, from, requester, requested_at}`; Home's My work (`GET /api/work`) lists `to_decide` in its
  approvals group as items of kind `approval_request`.

Everything is checked when the customization is saved, each fault naming the transition and the step: an unknown
role, field, state, transition, business calendar or time, or an expression that does not compile. Packs carry
policies as part of the workflow; `GET /api/customize/definition/{Entity}` returns the workflow with them.

* **Approvers read the record**: while an open step awaits someone's decision (or their stand-in's), they read that
  record (`GET /api/v1/{E}/{id}`), its lines, its history and its files even without a right to the entity; their
  field groups still apply unless the policy lists the field in `show` (`"show": ["salary"]`, none by default). Lists
  and searches do not show it to them, and they may do nothing else to it.
* **`lock: true`** refuses edits while a request is pending, but for `reset_on` fields.
* **Reminders and escalations made**: each step of `GET …/approval` carries `remind_at`, `escalate_at` (due) and
  `reminded_at`, `escalated_at` (done, else null). Each Item of `GET /api/approvals` also carries `requester_name`
  and `step` `{index, label, opened_at}`: the open step awaiting the caller in `to_decide`, the first open one in
  `awaiting_others` (`index` into `steps`, from 0).
* **Away**: `POST /api/away` sets one (above); `GET /api/away` → `[{starts_on, ends_on, current, substitute, substitute_name, reason}]`, the caller's
  periods today and to come; `DELETE /api/away` → `{cleared: n}` takes them back (`?starts_on=` one of them). An
  administrator adds `?user=` for anyone; anyone else gets 403 for another person.

### Approval before a document is submitted or cancelled

A workspace attaches a policy of the same shape to a submittable entity's submit, and separately its cancel, where no
workflow submits it (decisions.md BD-19, amended):

```json
{"entity": "PurchaseOrder", "op": {"op": "set_approval", "on": "submit", "approval": {
  "steps": [{"label": "Buyer's manager", "approvers": {"role": "Purchase Manager"}, "need": 1,
             "when": "doc.grand_total > 50000", "remind_after": "PT8H"}],
  "reset_on": ["grand_total", "supplier"], "lock": true, "system_bypass": false, "show": []}}}
{"entity": "PurchaseOrder", "op": {"op": "set_approval", "on": "cancel", "approval": {
  "steps": [{"label": "Finance", "approvers": {"role": "Accounts Manager"}}]}}}
{"entity": "PurchaseOrder", "op": {"op": "set_approval", "on": "cancel", "approval": null}}
```

`approval: null` takes it away. Every submit or cancel but a workflow's asks (`POST …/submit`, create-and-submit,
MCP, an app's hook, an automation): the caller must be able to do it now and, for a submit, the document must pass its
rules and the app's validation; the answer is the document as it is with `_approval` (`document: true`, `action:
"submit"`). When the policy is satisfied it completes once, **as the requester at that moment**: their right is
checked again and the app's hooks run as theirs. Refused then, nothing is submitted and the request's `status` is
`failed` with `failure` saying why (history `approval:failed`). `on_reject` and `not_who_took` are refused here (no
states, no transitions); `system_bypass` lets the system go ahead without asking. `GET
/api/customize/definition/{Entity}` returns them as `approvals: {submit, cancel}`, and what the packages declare (a
package workflow's transitions) as `package_approvals: [{on, policy, source: "package", package}]`; for a package's
entity, which has no definition to replace, it answers `{entity, package, approvals: [those]}`. `GET /api/meta/{Entity}`
says to whoever may submit or cancel whether each asks for approval first: `approvals: {"submit": {"asks": true,
"steps": ["Manager"]}, "cancel": {"asks": false, "steps": []}}` (the steps' labels, no approver named; an action the
caller may not take is left out).

### Approval before an action runs

The same policy on one of an entity's actions (a button's effects or an app's handler), `on: {action: <name>}`, by the
workspace or in a package's `approvals:` ([BD-34](builder/decisions.md#bd-34-approval-before-an-action-runs)):

```json
{"entity": "Change", "op": {"op": "set_approval", "on": {"action": "implement"}, "approval": {
  "steps": [{"label": "Change board", "approvers": {"role": "Change Board"}, "need": 2}]}}}
```

Refused for an action the entity lacks or a `read_only` one. `POST /api/v1/{E}/{id}/actions/{name}` by someone who may
run it now answers `{"_approval": {…, "on_action": true, "action": "implement", "params": {…}}}` and runs nothing; a
secret parameter shows as `[redacted]` there and in the history, its value sealed with the server's key (without one, an
action given a secret is refused). When the last step approves, the action runs once **as its requester at that
moment**, with exactly the parameters asked; refused then (their right gone, the record no longer meets the action's
condition or a refusal), the request ends `failed` with `failure` in the action's words and the requester is told. A
rejection or a recall runs nothing; a transition or a submit of the record does not end the request. Customize lists it
as `approvals: {"action:implement": …}`.

## Public sign-up

Off unless an administrator turns it on for a portal role: `PUT /api/admin/roles/{role}/signup {domains?: [...], passes?:
[...]}` (`null` turns it off; refused for a role that is not a portal role). Then someone with no account posts `POST
/api/auth/code/sign-up {tenant, role, name, email, passes?}` and is mailed a code; the code sign-in (`POST
/api/auth/code/sign-in`) confirms it, and only then are they a portal person of the workspace holding the role, signed
in. The answer is `{"sent": true}` whether or not the address is someone's: a portal person is sent the usual sign-in
code instead, anyone else nothing. `domains` (when given) are the e-mail domains it is for; `passes` the names of what a
sign-up page may pass on (a course, say), each a word, a number or yes/no of at most 200 characters, anything else
refused. On confirmation `ferrum.portal.signed_up` is raised with `{user, email, name, role, passed}` (an app enrols
them from it) and the audit trail has `User` `portal_signup`. Requests are limited to 3 an hour per address and 20 an
hour per network address per workspace (429), counted in the database so the limits hold across replicas; requests
are kept a day. A code works once, for 10 minutes, five tries; a code sent before sign-up was turned off makes nobody.

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
| `GET /api/v1/{E}/{id}/attachments` | → `{attachments: [{id, file_name, content_type, size_bytes, created_by, created_at, url, shared}]}` (what an outside person uploaded is always `shared: true`); `DELETE /api/files/{id}` removes one (an attachment only by someone who can edit its record; a loose upload by its uploader) |
| `GET /api/users` | directory of active users `[{id, full_name, email}]` for mentions and assignment |

**Shared with outside people** ([BD-22](builder/decisions.md#bd-22-comments-and-files-shared-with-outside-people)).
Comments and attachments carry `shared` (false unless staff choose). `POST /api/v1/{E}/{id}/comments` takes `shared:
true`; `PUT /api/comments/{id}/shared` and `PUT /api/files/{id}/shared` `{shared}` change it (a member who can read the
record; `204`). A portal role's rule says per entity what its holders see: `{"entity": "Ticket", "read": true, "via":
"email", "shared": "read" | "add"}`. With it, a portal person's `GET …/comments` and `…/attachments` list the shared ones
(and, for files, their own), without the authors' addresses; with `add` they comment and attach (`POST …/comments`,
`POST /api/files` with the record), always shared. Without it they are refused (`403`), as before. A download of a file
not shared with them answers `404`, as a file that is not there. Sharing a comment tells the portal people who may see it
with the template `core.comment_shared` (`values.record`, `values.comment`); an outside person's comment notifies the
record's followers.

## Signed action links

"A person with this link may do this one thing to this one record" ([BD-23](builder/decisions.md#bd-23-signed-action-links)).

| Method & path | Notes |
|---|---|
| `POST /api/v1/{E}/{id}/links` | staff: `{actions: [name] \| ["view"], show?: [field], columns?: {table field: [column]}, params?: {name: value}, expires_at?, uses?: 1-100, code_to?: address or number, recipient?, frame_origins?: [origin]}` → `201 {id, url, expires_at}`; the `url` holds the token and is shown only now. A table field in `show` needs 1 to 8 `columns`, each a field a public page may show; its rows are shown read-only |
| `GET /api/v1/{E}/{id}/links` | `{links: [{id, actions, show, columns, uses, max_uses, expires_at, recipient, code, revoked_at, created_at, issued_by, opened_at}]}`, never a token |
| `DELETE /api/links/{id}` | revokes it (`204`) |
| `GET /api/public/{workspace}/links/{token}` | the page, no session: the record's kind and title, the fields `show` lists, one form per action asking its own parameters (not those fixed), and the code step when the link has one |
| `POST /api/public/{workspace}/links/{token}` | a form: `_send=1` sends the code; `_action`, the action's parameters and `_code` run it once. The answer is a page: done (`core.link_done`), the form again with what was wrong (`422`), or gone |

A link that does not work (unknown, expired, revoked, used up, another workspace's, a workspace that is not there)
answers `404` with the page `core.link_gone` as the platform ships it (never a workspace's edit, so the answer is the
same whether the workspace exists), whatever the reason. The first `POST` to a working link's page records
`opened_at` once and raises the record's `opened` event (`link: {id, recipient}`; automations take `event: "opened"`);
reading the page does not, so a link preview or a mail scanner opens nothing
([BD-38](builder/decisions.md#bd-38-signed-links-ended-by-an-app-dated-by-the-record-opened-with-lines)). Pages are never cached and send no referrer. Every request counts against the
public limiter (per address and workspace, kept in the database).

The effect `{"type": "link", "actions": [...], "show"?: [...], "columns"?: {...}, "params"?: {name: expression}, "days"?:
n, "until"?: expression, "uses"?: n, "code_to"?: expression, "recipient"?: expression, "frame_origins"?: [...]}` issues one
on the record as the system and gives `{id, url, expires_at}`; send it with a `message` effect: `"values": {"url":
"results[0].url"}, "secret": ["url"]`. `until` gives a date (the link works through that day, UTC) or a moment, the
record's valid-until say; with `days` too the sooner; never past 90 days; one already past is refused. An app calls
`Kernel::issue_link(ctx, tx, entity, id, &LinkSpec)` in its own transaction, and ends a record's links in it with
`Kernel::revoke_action_links(tx, ctx, entity, record, Some(action) | None)` (how many it ended).

**Files and own words** ([BD-44](builder/decisions.md#bd-44-secrets-in-every-form-files-and-own-words-on-a-links-page)).
An action's parameter `{"name": "photos", "label": "Photos", "type": "files", "max_files"?: 1-10, "max_bytes"?: ≤ 10485760,
"accept"?: ["image/*", "application/pdf"]}` (one file of 10 MB without them) is a file box on the page, whose form then
posts as `multipart/form-data` (25 MB a post at most): each file is held to the parameter (`422` naming it), scanned and
stored as an outside person's upload, attached to the record, shared, in the use's transaction, and given to the action
as the files' ids; a refused action discards them. An app calls `Kernel::use_link_with(…, &[LinkFile])`. An action's
`condition_message` (`"{{ doc.title }} was accepted already"`, rendered as a rule's message over the record as its
reader may read it) is what a refusal for its `condition` says, on the page and through the API, instead of the
general words.

**A secret value in a message** (`"secret": ["url"]`) is masked, wherever rendered text is kept or logged, in each form
it may be printed in: as it is, HTML-escaped, percent-encoded, as JSON, and drawn by `qr` or `barcode` with their
defaults.

**Rendering for another transport** ([BD-45](builder/decisions.md#bd-45-quorums-from-the-record-absent-parameters-targets-on-every-write-rendering-for-another-transport-and-the-app-api-034)):
`messages::render_for(k, tx, ctx, &Message, "email" | "text" | "chat")` → `{channel, address, language, subject, html,
text, opt_out, list_unsubscribe: (List-Unsubscribe, List-Unsubscribe-Post)}`, as `send` would render it for its one
recipient, nothing sent; `403 opted out` for an address that opted out (an erased one's digest included).
`Kernel::opted_out(tx, tenant, channel, address)` asks the same. An approval step's `need` may be an expression over
the record (`"doc.quorum"`), kept on the request when it is made, between one and the approvers found. An optional
action parameter not given stays absent in its effects and refusals: ask with `has(params.x)`.

**Calls for apps** ([BD-43](builder/decisions.md#bd-43-calls-apps-were-missing)), each in the app's own transaction and
asking what its route asks: `Kernel::tag_tx(ctx, tx, entity, id, add, remove)` → the record's tags;
`Kernel::ensure_calendar_tx(ctx, tx, &Calendar)` → whether it made the working calendar (one of the name is left as it
is); `Kernel::attach_files_tx(ctx, tx, ids, entity, record)` → each file's `{id, file_name, content_type, size}`, the
caller's loose uploads attached, files already on the record passing, anything else refused.

### Replacing an app's own guest link with a signed action link

Eight apps keep guest links of their own (a token table, a hash, a page that checks it). To move one:

1. Make what the guest does an action of the entity (`actions:` in the package's YAML) with its `params`, its
   `condition` and, where needed, `refuse_if`. Keep its handler in the app's hook, or give it `effects`. It must not be
   limited to `roles`.
2. Where the app minted its token, call `kernel.issue_link(ctx, tx, entity, id, &LinkSpec { actions, show, params,
   expires_at, uses, recipient, .. })` in the same transaction, and put the returned `url` in the message's values,
   naming it secret.
3. Move the page's wording to the templates `core.link_page`, `core.link_done`, `core.link_gone` (a workspace edits
   them) and the fields it showed to `show`.
4. Keep the old route answering the links already sent until the longest of them expires (Bookings: 30 days after a
   booking), then delete it, its token table and its tests' token cases; the platform's are in `action_links_it`.
5. Say it in the app's changelog: links sent before keep working until they expire; new ones are the platform's.

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
  lists them, heads first). An app's `create` row naming an `entity` is left out where the workspace lacks that
  entity (its package is not installed) or the person may not create it. Apps without an interface of their own, without launcher entries of
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
`/api/admin/roles` (GET; each role with `portal`), `/api/admin/roles/{role}/portal` (PUT `{portal}`: marks a role for the portal, refused while a rule of it reaches every entity, deletes, cancels, exports or imports, or reaches every record other than to read reference data, an entity holding no personal data; portal people hold only such roles), `/api/admin/roles/{role}/permissions` (PUT PermissionDef `{entity, read, write, create, delete, submit, cancel, export, import, if_owner, read_groups[], write_groups[], where?, via?, actions?, shared?, hide?}`; the role list gives each rule back with `hide` and `packaged`, whether its package still rewrites it);
`hide` ([BD-40](builder/decisions.md#bd-40-a-rule-that-does-not-show-some-fields)) names fields and child tables the rule neither shows nor accepts: a record read (one record, a list's rows, `fields=`, a record page, a print) leaves out a field every rule reaching that record hides, and shows it when another rule reaching it does; filters and sorting on a field some rule hides and no rule reading every record shows are refused (`403`), and search, reports, exports and history leave it out; a write naming it is refused (`422` on the field) whatever its value. Refused when saved: a field the entity lacks, its title field, a field of a unique key, a field of the rule's `where` or its `via`'s first step, and any `hide` on `*`. A package's role may carry it; a rule row follows the releases of the package that wrote it, and of no other (a second package declaring a rule for the same role and entity leaves the first one's row as it is), until an administrator edits it here;
`via` makes a rule reach the holder's own records through a path to their email (`patient.email`, `student.guardians.email`: at most two links, then optionally a child table, then an email or text field), checked against the entities it crosses when saved and on every use; `actions` names the entity's actions its holders may run on the records it reaches, whatever the action's own permission asks (its condition and its app's checks still apply);
`where` limits a rule to records whose fields hold given values, all of them: `{"customer_type": ["Individual"], "disabled": false}` (data, select, check, link, email and int fields; links by record id, an unset checkbox reads as `false`; up to 10 fields and 50 values each). A role can hold several rules for one entity with different conditions (and `if_owner`), and a record is reachable through any of them, in lists, reads, filters, reports and live updates alike;
`/api/admin/packages` (GET, POST `{packages[]}` → `{installed, kept_newer: [{name, version, build}]}`: a package the workspace holds newer than this build's (a server rolled back) is left as it is, its version, roles and seed data untouched, and listed in `kept_newer`; the others install; `ferrum task doctor` reports it as `packages.newer`); `/api/admin/webhooks` (GET, POST `{name, url, topics[]}` → `{id, secret}`, DELETE `/{id}`; a workspace's own webhook receives a record event's payload with `doc` and, on a change, `was` next to it; an app's webhook receives neither, only which record);
`/api/admin/automations` (GET, POST `{name, entity, event: created|updated|submitted|cancelled|deleted|transitioned|scheduled, schedule?, condition?, actions: [...] , is_active}`, DELETE `/{id}`).
An automation's `condition` (CEL) sees the record as `doc`; on a change of an existing record (`updated`, `submitted`, `cancelled`) it also sees `old`, the record before the change, so `doc.status == 'Closed' && old.status != 'Closed'` holds once. On `transitioned` (a workflow transition within one document status) it also sees `transition`: `{action, from, to, by}`; the event's payload carries the same. The event's payload carries `doc` (up to 256 KiB) and, with it, `was`: the previous value of each field the change replaced (not tables), left out when the two together pass 256 KiB.
An automation fires on a record's event, or on time: with `event: "scheduled"` it carries a `schedule` (and only then), in one of two forms.
`{"relative": {"field": F, "offset"?: "-P3D", "at"?: "09:00", "repeat"?: "P1D"|"P1W"|"P1M"|"P1Y", "until"?: G}}` fires for each record at its date or datetime field `F` plus `offset` (a signed ISO-8601 duration of days, hours and minutes: `-P3D`, `PT2H`, `-P1DT30M`; at most five years either way); a date field's moment is at `at` (default 09:00) in the workspace's time zone, a datetime field's at its own time. With `repeat` it fires again every day, week, month or year after that (a month after 31 January is 28 February, then 31 March), up to the record's date or datetime field `G` (`until`, a date counting whole) when given.
`{"every": {"period": "day"|"week"|"month", "at": "HH:MM", "on"?: 1-7 (a week, Monday 1, default 1) | 1-28 or "last" (a month, default 1), "filter"?: [[field, op, value], ...]}}` fires at each period's moment for every record that passes `filter` (list-filter form over the entity's own fields, literal values, at most 8 conditions, all of them).

**By absence** ([BD-27](builder/decisions.md#bd-27-schedules-by-absence)): `"absent": {"entity": "XTimesheet", "link":
"person", "date": "work_date", "within": "week" | "month" | "P{n}D" (1 to 366), "filter"?: [...]}` in `every` fires only
for the records that no record of `entity` whose `link` field names them, passing its `filter`, has a `date` in the
period around the run (the week or month it falls in, or its last n days, today included), and at most once a period
for each record (the occurrence is keyed by the period's start; n days are counted in blocks from 1 January 1970). At
the run the record is judged again: one that passes no more, or now has what it missed, is skipped.
Moments are found within a minute by a scan on every server, and each (record, moment) fires once however many look. A new automation, or one whose schedule or entity changes, or one switched on again, starts from now: no moment already past fires. A moment fires with the record as it is then, read with the rights of whoever saved the automation: it does not fire if the record is gone, its date has moved away or been cleared, or it no longer passes the filter, and `condition` is judged on it then. A date moved to a new moment fires again for that moment; one moved into the past does not. After the scan has been stopped for more than seven days, the older moments are skipped (logged with how many), not fired late. A scheduled run's changes count one step in an event chain, as an event's do. Each moment queues an `<Entity>.scheduled` event carrying `{automation, record, due_at}`, which webhooks subscribed to the entity's events also receive.
Automation actions: `{"type":"set_fields","values":{field: CEL}}`, `{"type":"create_document","entity":E,"values":{field: CEL}}`, `{"type":"update_linked","link":F,"values":{field: CEL}}`, `{"type":"notify","to":"owner|role:<name>|user:<id>|field:<name>|links:<link>.<table>.<field>","title":CEL,"body"?:CEL}`, `{"type":"email","to":CEL,"subject":CEL,"body":CEL,"template"?:"<jinja html>"}` (or `"message":KEY` in place of `subject` and `body`, and `"message":KEY` in place of `notify`'s `title`: a [message template](#messages)), `{"type":"message","template":KEY,"to":[…],"channels"?:[…],"all"?:bool,"values"?:{name: CEL}}` ([messages](#messages)), `{"type":"invoke","capability":ID,"input":{name: CEL},"key"?:CEL}` (another app's step through the capability gate, below, "Calling another app's steps").
Each action after the first sees what the ones before it gave as `results` (as a configured action's effects do, below).
**Effects on lines**: `{"type":"append_line","table":T,"values":{field: CEL}}` adds one line at the end of the record's table field `T`; `{"type":"set_line","table":T,"where":CEL,"values":{field: CEL}}` changes the one line for which `where` holds, an expression over `row` (the line) besides `doc`, `user`, `params`, `results` and `links`, which its values also see. No line, or more than one, refuses the run and says which (`set_line: no line of reviews has row.stage == params.stage`; `set_line: lines 3, 4 of reviews all have …`). Both write through the record's ordinary save: one new version per effect, computed fields, rollups, row rules, checks, Logic rules and the audit; every other line keeps its id and values, and the line changed keeps the fields not named. A submitted record refuses them (`a submitted Application cannot be edited; cancel and amend it instead`), and the cap on lines holds. Checked when saved: `T` is a table field of the entity, and each field a field of its lines that an effect may write. They are the only effects that change a table.
`update_linked` writes the record the link field `F` of the triggering record names: that record is locked and saved through its own pipeline (its computed fields, checks, Logic rules and unique keys; its events and automations follow), with the rights of whoever saved the automation, as `set_fields` is (a read-only field of it is not written). Its values see `doc` (the triggering record) and `linked` (the record written, as it was before). An empty link does nothing; a link naming a record that is not there fails the run. `F` must be a link field of the entity and each field written a field of the linked entity, checked when the automation is saved; the chain rule holds, the linked record's change being one step further in the chain.
An e-mail's `body` gives text: it is escaped and its line breaks kept, so nothing a record holds (typed into a public form, say) becomes markup in a mail sent in the workspace's name. For formatted mail, a `template` (MiniJinja, the engine and limits of print templates, at most 64 KB) makes the HTML in place of `body`: its own markup stays, every value it prints is escaped unless the template itself marks it `safe` (never do that with text people typed), and it sees `doc` (the record) and `user` (whom the automation runs as), as the expressions do; one that does not compile is refused when the automation is saved. The `ferrum.mail` capability takes text only.
A refused action is reported as a field error on `actions` with the action's place as its row, the same `row` that errors in a document's child rows carry: `{"field": "actions", "message": "...", "row": ["actions", 1]}` is the second action (counting from 0), so an editor can show the message on that action.
`POST /api/admin/automations/preview {entity, body?, template?, record_id?}` (administrators only) renders what an e-mail action would send, as HTML, without sending or writing anything: the `template` when given, otherwise `body` (an expression) as escaped text. It uses the record `record_id` names, read with the caller's rights (not found or not readable: 404), or when none is named the most recently changed record the caller may read, or an empty record when the entity has none. The answer is `{"html": "...", "record_id": "<id>" | null}`. A template that does not compile or fails, or an expression that fails, is a 422 field error on `template` or `body` with the engine's message; a template over 64 KB is refused as when saving.

Customization: `GET /api/customize` → `{customizations: [{id, seq, entity, op, is_active, note, created_at, superseded_by?}], warnings}` (`superseded_by`, on an active customization the schema no longer applies, the id of the one that replaced it: a later active `add_action` or `remove_action` of the same action, a later active `define_module` or `remove_module` of the same menu, a later active `publish_list` or `unpublish_list` of the same list, or a later active `redefine_entity` of its entity, below);
`POST /api/customize` `{entity, op, note?}`; `PUT /api/customize/{id}/active` `{active}`; `GET /api/customize/export`; `POST /api/customize/import`.
Ops: `{"op":"add_field","field":Field,"after"?:name}` (custom field names start with `x_`; on a package's master, document or settings entity the field may be a table of a custom child entity, below), `{"op":"set_property","field","property","value"}` (label, description, required, hidden, read_only, in_list, in_filter, default, default_expr, visible_if, required_if, read_only_if, read_only_message, max_length, min, max), `{"op":"add_rule","rule":{name, check: CEL, message, on: save|submit|transition, field?}}` (`on: transition`: only when a workflow transition is taken, on the record in its new state, reading `transition`: `{action, from, to, params}`; "this check applies only when approving" is `transition.action != 'Approve' || …`, and the fields a state needs are its `required`) (`message` may show the record's values, `{{ doc.title }}`, as its reader may read them; `field` names the field the refusal is shown on, by default the rule's name), `{"op":"add_options","field","options":[...]}`, `{"op":"set_entity_property","property":"label|label_plural|description|icon|title_field|list|search_fields|form|numbering","value"}` (`numbering`: `{template, gapless, on: save|submit, date_field}` with `{seq:<width>}` in the template; a gapless series stays gapless), `{"op":"define_entity","entity":EntityDef}` (name starts with `X`; a master, a document, a child entity, or the workspace's settings (`"kind": "settings"`: one record for the workspace, a second refused, read in expressions as `settings.<Entity>.<field>`, docs/builder/expressions.md "Settings", BD-17); a master or document may have table fields whose rows are a custom child entity, each child entity the rows of one table field only, and such a child entity may itself have table fields whose rows are another custom child entity (lines within lines: a record, its lines, their lines, and no third level, refused as `lines go two levels deep (a record, its lines, their lines), and XC2 has lines of its own`), and unique keys, `unique: true` on a field or `unique: [[fields]]`, which a child entity may not; a save that would repeat another record's key is refused with 409 naming the fields; a cancelled document and empty values hold no key; defining a key over records that already share a value is refused, listing them), `{"op":"set_workflow","workflow":WorkflowDef}`, `{"op":"set_print_template","template":"<jinja html>"}`, `{"op":"redefine_entity","entity":EntityDef}` (below), `{"op":"add_action","action":ActionDef}` (a configured action, below), `{"op":"remove_action","name"}` (below), `{"op":"define_module","module":{...}}` and `{"op":"remove_module","name"}` (a workspace's own menu, below), `{"op":"add_target","target":TargetDef}` and `{"op":"remove_target","name"}` (service targets, below), `{"op":"publish_list","list":{...}}` and `{"op":"unpublish_list","name"}` (published lists, below).

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

* Who may run it: the permission it names (`write` by default) on the record, or a role's rule naming it, as for any action; then, when `roles` is given, one of those roles (an administrator always may), else 403 `'Accept It' is for Risk Approver`; then `condition` (otherwise 422, as today); then each `refuse_if` in order, whose first `when` that holds (CEL over `doc`, `user`, `params` and `links`) refuses with 403 and its `message` (which may show the record's values, `{{ doc.title }}`, as the person pressing may read them).
* `params` are the action's parameters (the request body, an object), `params.<name>` in its expressions.
* `effects` are automation actions (`set_fields` on the record itself, `update_linked`, `create_document`, `notify`, `email`, `invoke`, `append_line` and `set_line` on its lines), run in order inside the action's one transaction, as the system on the runner's behalf, each through the save pipeline every write uses: the record's computed fields, rollups, checks, Logic rules, workflow rules and unique keys apply to what they write, and the usual record events and automations follow after commit. `set_fields` writes read-only fields (a status moved only by buttons); records written say the runner changed them. Each effect sees the record as the effects before it left it. Any refusal (a check, a rule, a key, a missing linked record) undoes the whole action and reaches the caller in its own words. The audit trail records `action:<name>` with the runner and the parameters.
* What an effect may write: a custom entity's fields, a packaged entity's `x_` fields (what a Logic derivation may fill), and a packaged entity's fields a person may type; never the platform's columns (the reserved names `id`, `tenant_id`, `version`, `docstatus`, `number`, `created_at`, `created_by`, `updated_at`, `updated_by`, `amended_from`, `workflow_state` and the like), a computed or rolled-up field, a secret, a table (except one line of it through `append_line` or `set_line`), a package's read-only field or a field in a permission group. No effect runs another action.
* Checked when the customization is saved, each refusal naming the entity, action and effect: expressions compile, fields and links exist where they are written, the above holds; at most 20 configured actions on an entity, 10 effects and 10 `refuse_if` on an action; a configured action is not `read_only`, and `roles` and `refuse_if` go with `effects`. A later customization that would leave one writing a field that is gone is refused.
* The answer is `{status?, made?}`: the record's `status` after it, and `{entity, id, number}` of each record `create_document` made, from which the usual message is derived ("Now Accepted; made a Log"). An e-mail's recipient, subject, body and template all see `params` (`{{ params.note }}` in a template).
* Each effect after the first sees what the ones before it gave as `results`: `results[n]` is the nth effect's (counting from 0), `{entity, id, number}` for `create_document` and `update_linked`, the receipt's result for `invoke`, null for the others. So "create it, then keep its id here" is two effects: `{"type": "set_fields", "values": {"invoice": "results[0].id"}}`.
* A value that is only a path to a value (`doc.fee`, `linked.price`, `links.plan.price`, `row.amount`, `params.x`, `results[0].total`: no operator, no call, no brackets around it) copies it as stored, so an empty number, date or tick stays empty, where any other expression reads an empty number as 0 and an empty tick as false (`doc.fee + 0` writes 0); this holds for `set_fields`, `create_document`, `update_linked`, `append_line` and `set_line`, in automations too.
* **Effects that branch, find and loop** (decision BD-18; automation actions alike; `crates/kernel/tests/effects_it.rs`):
  * `"when": CEL` on any effect or automation action: it runs only when that holds (an expression over what its values see); without it, always. A skipped effect gives null in `results`.
  * `{"type":"find","entity":E,"by":{field: CEL},"create"?:{field: CEL},"update"?:{field: CEL}}`: the one record of `E` whose `by` fields hold those values (none may be empty: `find: code is empty, so it picks no Party`). None: with `create`, one is made with `by`'s values and `create`'s, otherwise nothing (null). One: with `update`, it is changed (its values see it as `linked`). More than one refuses the run (`find: more than one Party has code P-1: it picks one record, never a guess`). It gives `{entity, id, number}` in `results`. `by: {"id": "item.id"}` picks a record by its id. Runs that may make the same record (a `create` with the same values) wait for each other in their transactions, so two at once make one; a unique key over `by` refuses a duplicate made any other way, and without one a record made by hand meanwhile makes the next find refuse.
  * `{"type":"for_each","table":T,"where"?:CEL,"effects":[...]}` runs its effects once per line of the record's table `T`; `{"type":"for_each","entity":X,"link":F,"where"?:CEL,"effects":[...]}` once per record of `X` whose link field `F` names this record (oldest first); `{"type":"for_each","links":"L.T",...}` once per line of table `T` of the record link field `L` names (read as the workspace, as `links` is; nothing when `L` is empty). The item is `item` in their expressions and `where`'s; they see the record as it was when the loop began. At most 100 items (`automation::MAX_ITEMS`), counted before `where`; more refuses the run (`effect 1 (for_each): more than 100 lines of steps, the most one loop runs over; nothing was done`). 1 to 10 effects per item, no loop within a loop; an `invoke` in a loop has an effect key per item. It gives each item's results, in order. To change the line at hand: `{"type":"set_line","table":T,"where":"row.id == item.id",...}`; a linked record: `{"type":"find","entity":X,"by":{"id":"item.id"},"update":{...}}`.
  * `{"type":"copy_lines","table":T,"link"?:F,"result"?:n,"from_table":S,"values":{field: CEL},"mode"?:"append"|"replace"}` copies the lines of table `S` of another record (the one the link field `F` names, or the one effect `n`, an earlier `find` or `create_document`, gave) into the record's table `T`, each value an expression over `item` (the line copied: `"due": "addDays(doc.start, item.offset_days)"`), in one save of the record. `append` (the default) keeps the lines there are. An empty link or a null result copies nothing; more than 100 lines refuses the run.
  * A refusal of any effect says which and what kind it is: `effect 2 (set_fields): Title is required`, `effect 1 (find): more than one …`, `effect 3 (for_each): for_each, item 2, effect 1: …`; the whole run is undone. Checked when saved, with the effect's place: entities, fields, tables, links and `result` exist and fit, what is written may be written by an effect, the message and document templates a `message`, `notify`, `email`, `generate` or `report_documents` names are there (a package's or the workspace's: `effect 2: there is no message template wk.nothing`), and a `message`'s `field:` recipient is a field of the record.
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
* **By record** ([BD-41](builder/decisions.md#bd-41-service-targets-by-record)): `"calendar_field": "agreement.calendar"` (a data or select field of the record, or a link and such a field of the linked record, read as the workspace) names each record's calendar, the target's `calendar` when it is empty; `"start_field": "opened_at"` (a date-time field) starts the clock at that moment rather than at the save, unless it is empty or still to come, so an imported ticket is due by its own opening (and breached at once, once, when that is long past). A change of either on a running round works its due moment out again once: time counted so far stays as it was counted, the rest counts on the new calendar from the change, a start moved adds or takes away the open time between; pauses and a breach stay. A calendar a record names that the workspace lacks refuses the save.
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
* **Record pages, search and paging** ([BD-25](builder/decisions.md#bd-25-public-pages-for-published-records)). `"slug":
  "path"` (a text field, held to the rules above) gives each published record a page at
  `/public/{workspace}/lists/{name}/{value}`; `"detail": [...]` (the same rules, at most 12) are fields only that page
  shows; `"summary"` (one of the fields shown) is its description for search engines. A list with a slug takes `?q=`
  (its own text fields, any case) and `?page=` (rows a page: its `limit`; at most 100 pages), on the page and on
  `GET /api/public/{workspace}/lists/{name}` (which then adds `addresses`, each row's slug; its JSON always says `page`,
and `q` when searched; a page past the last is a `404` there too). It is offered to
  search engines: `index, follow`, a canonical link, and `/public/{workspace}/sitemap.xml` of every published record's
  page; a list without a slug stays `noindex`. Every answer carries an `ETag` of its content (`304` when unchanged) and
  `Cache-Control: public, max-age=60`. A draft, cancelled or filtered-out record, a list without a slug and a page past
  the last are the same 404.
* **Views and votes** ([BD-42](builder/decisions.md#bd-42-public-pages-count)): `"count_views": "views"` and `"votes":
  {"up": "helpful", "down": "unhelpful"}` (whole-number fields of the entity, on a list with a slug) count, on each
  record page, the visitors who saw it and their "was this helpful" answers. The page posts `do=view` back to its own
  address from one script its policy allows by digest (a fetcher reading the page counts nothing), and its two answers
  post `do=up` or `do=down`: `POST /public/{workspace}/lists/{name}/{value}`, from the page's own origin only (`403`
  otherwise), `204` for a view and `303` back to the page for a vote, the one `404` for anything not counted. A view
  counts once a visitor a record a day, a vote once a visitor a record (another answer moves it); a visitor is a
  random first-party cookie (`ferrum_visitor`), kept only as a keyed digest; an address counts at most 60 times an hour
  a workspace (`429` past it). The counters change without a new version, an audit entry or an `updated` event, and
  what anyone sends for them is ignored.
* **Framing** ([BD-24](builder/decisions.md#bd-24-frame-headers-per-page)). `"frame_origins": ["https://town.example"]`
  (at most 10, `https://host[:port]`) lets those sites frame the list's pages; without it they send `frame-ancestors
  'none'` and `X-Frame-Options: DENY`. An action link takes `frame_origins` alike, and an app's public page gets them
  from `Kernel::set_public_frame(ctx, app, "forms/f/{form}", origins)` (an administrator or the app as the system), or a
  site page from its answer's `x-ferrum-frame-ancestors` header; every other page stays unframeable.
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
- `POST /api/admin/pack/install` `{pack, digest}` → `{id, name, version, digest, summary, failed, warnings}` (`warnings`: the customizations of the workspace it left replaced, as the plan names them). Refused (422 on `digest`) unless `digest` is the pack's, so the plan an administrator saw is the one applied; refused (409) when something is missing. Everything but the solutions is applied in one transaction, through what the admin API uses, so every check applies and a refusal anywhere leaves nothing: calendars (one of the same name here is left as it is), the pack's roles by name and its message and document templates (so what the customizations name is there), customizations as an import (validated as a whole, exact duplicates skipped, a custom entity this workspace defines otherwise redefined under `redefine_entity`'s guards), roles and their rules (a portal role marked before its rules), automations (matched by name, saved as the installer), Studio's items (matched by entity and label, references found by label here), records (matched by key, created or updated, never deleted). Then each solution is taken in as a draft (`POST /api/app/studio/solutions/import/pack`'s rules: it runs only once kept as a release, reviewed by a second administrator and activated); one that fails is listed in `failed` with why, and installing again is safe. Installing the same pack again changes nothing. Each install is kept (`pack_install`) with what it did and audited (`Pack` `install`).
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
`{backfills: [{package, name, state (waiting|running|done|failed), changed, remaining, started_at, finished_at, needing_now, error}]}`;
`POST /api/admin/backfills/{package}/{name}/rerun` runs a finished one again at the next upkeep, and one whose job
failed at once from where it got (409 while it runs). A failed one is also queued again, once, when its package's
installed version changes ([BD-39](builder/decisions.md#bd-39-faults-a-walk-of-a-live-server-found)).

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
one of them; `PUT /api/admin/calendars/{name}` `{zone, weekends, holidays, hours?, exceptions?}` → `{revision}`, each
change a new revision; `DELETE /api/admin/calendars/{name}`. `hours` is windows per weekday in the zone,
`{"mon": [["09:00", "13:00"], ["14:00", "18:00"]]}`; left out, every hour of a business day is open. Bad
hours are a field error on `hours`. `exceptions` say what given local dates are, whatever their weekday and
holidays say: `{"2026-12-24": {"open": false, "hours": [["12:00", "24:00"]]}, "2026-12-27": {"open": true,
"hours": [["10:00", "14:00"]]}, "2026-12-31": {"open": false}}`. Open, with windows (all day without them), opens a
holiday or a weekend day and replaces a weekday's hours; closed, all day, or only those hours of the day's own
windows. A date opened is a business day, one closed all day is not. At most 1,000; bad ones are a field error on
`exceptions`. Every calendar function, wait, service target and approval step's time counts them; packs carry them. These are the functions Studio's `/calendars` route calls. Deleting a
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

`AppSummary` = `{id, name, version, runtime: native|wasm|meta, summary, categories[], publisher?, verified, icon_url, source: native|upload|store:<url>, private, parent?, requires: [{app, range}], platform: <range>|null, blocked: [{app, need, have, action: install|upgrade}]}`.
`requires` and `platform` come from the app's manifest (a store's search result carries neither: empty and null). `blocked` is what keeps it from installing in this workspace now, worked out against the apps it has: an app it needs that is not installed (`install`, `have: null`) or installed outside the range (`upgrade`), and `{app: "platform", need, have: <this server's App API version>, action: "upgrade"}` when the server is outside `platform`; empty when nothing blocks. Both lists carry it.

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

## Connections

Connections to outside services: one described HTTP service a workspace calls, is called by, pulls from and signs in
to ([BD-29 to BD-33](builder/decisions.md#bd-29-one-described-call-for-everyone)). No provider is named in the
platform: a provider is a description.

**A connection** (`POST /api/admin/connections`, administrators):

```json
{"name": "carrier", "label": "Parcel carrier", "enabled": true,
 "secrets": {"api_key": "…"},
 "description": {
   "base": "https://api.carrier.example/v2",
   "auth": {"type": "bearer", "secret": "api_key"},
   "headers": {"x-account": "acme"},
   "allowed_hosts": ["labels.carrier.example"],
   "timeout_ms": 15000, "max_bytes": 1048576,
   "roles": ["Warehouse"], "apps": ["erp"],
   "operations": {
     "create_label": {"method": "POST", "path": "/labels", "inputs": ["weight", "ref"],
                      "body": "{\"weight\": {{ inputs.weight | json }}, \"reference\": {{ inputs.ref | json }}}",
                      "result": {"tracking": "/tracking_number", "label": "/label_url"},
                      "error_at": "/error/message", "temporary": [429, 503], "idempotency_header": "Idempotency-Key"},
     "track": {"path": "/tracks/{{ inputs.tracking }}", "result": {"status": "/status"}}
   }}}
```

`auth` is `{type: none}`, `{type: header, header, secret, prefix?}`, `{type: basic, username, secret}`, `{type:
bearer, secret}`, `{type: signed, header, secret, timestamp_header?, encoding: hex | base64, prefix?}` (HMAC-SHA256 of
`<timestamp>.<body>`, or the body) or `{type: oauth, …}` (below); each `secret` names one of `secrets`. An operation's
`method` is GET, POST, PUT, PATCH or DELETE; its `path` starts with `/` and every value it prints is percent-encoded;
`query`, `headers`, `body` (a template giving JSON) and `form` are templates over `inputs`, `secret` and `key`;
`result` maps names to JSON pointers (none: the whole answer); `changes` (every method but GET unless said) means it
needs a key. `{"test": true}` with a `fixture: {status, body}` per operation answers from that and calls nobody.
Calls go to `https` only, to the base's host and `allowed_hosts`, never to a private, loopback, link-local or reserved
address unless the operator names the host in `FERRUM_CONNECTION_HOSTS` (which also allows plain http to it).

| Method & path | Notes |
|---|---|
| `GET /api/admin/connections` | `{connections: [{id, name, label, description, enabled, secrets: [names], account, source, refused, debug_until, inbound}]}`; a package's connection not yet taken up has `id: null, offered: true` |
| `POST /api/admin/connections` | the body above → `201 {id}`; a wrong description is a 422 on `description` saying what |
| `PUT /api/admin/connections/{id}` | the same; `secrets` left out keeps the ones there are |
| `DELETE /api/admin/connections/{id}` | `204` |
| `POST /api/admin/connections/{id}/try` | `{operation, inputs, key?}`: runs it deliberately → `{ok, result, answer}` or `{ok: false, temporary, error}`, masked |
| `PUT /api/admin/connections/{id}/debug` | `{minutes}` (0 to 60): the call log keeps the requests and answers, redacted, meanwhile |
| `GET /api/admin/connections/{id}/log?limit=` | `{calls: [{at, operation, outcome, status, duration_ms, entity, record_id, error, debug}], received: [{delivery, received_at, state, error}]}` |
| `GET /api/admin/connections/{id}/pulls` | `{pulls: [{name, cursor, last_run, next_run, last_error}]}` |
| `POST /api/admin/connections/{id}/pulls/{name}` | pulls now → `202 {job}` (a person's own connection: one for each person connected) |
| `PATCH /api/admin/connections/{id}/secrets` | `{set: {name: value}, remove: [name]}`: the rest kept → `{secrets: [names]}`; audited by name |
| `POST /api/admin/connections/{id}/session` | a session connection signs in now, afresh → `{status, account, expires_at}` |

A pull answered "later" is tried again from where its cursor stood, with backoff, at most four attempts and only before
its next run; a run that fails (or runs out of attempts) keeps `last_error` and tells the workspace's administrators
once per streak of failed runs ([BD-36](builder/decisions.md#bd-36-a-failed-pull-is-tried-again)). A person's data
report and erasure reach the calls they made or that were about their records, deliveries and captures naming them, and
their own sign-ins, whose tokens go and are revoked at the provider where it says how
([BD-35](builder/decisions.md#bd-35-connections-in-a-persons-data)).
| `GET /api/connections` | a person's own: `{connections: [{id, name, label, status, account}]}` |
| `POST /api/connections/{id}/connect` | from the browser, signed in → `{url}` to send it to |
| `DELETE /api/connections/{id}/connect` | disconnects (the workspace's: administrators) → `{revoked}` |
| `GET /api/connections/callback` | where the provider sends the browser back; a page, no redirect |
| `POST /api/connections/inbound/{id}` | no session: a call from outside → `{accepted: true}`, or `401 this call was not accepted` whatever the reason |

**Calling** — an app: `ferrum_kernel::connections::call(&kernel, &ctx, Some("erp"), &Call { connection: "carrier".into(),
operation: "create_label".into(), inputs: json!({"weight": 2, "ref": "P-9"}), key: Some(format!("label:{id}")),
entity: Some("Shipment".into()), record: Some(id) })` gives the mapped result; `Unavailable` means "later", `Invalid` the
service's "no" in its words. The effect, on buttons, transitions, approvals and automations:

```json
{"type": "call", "connection": "carrier", "operation": "create_label", "inputs": {"weight": "doc.weight", "ref": "doc.number"},
 "key": "'label:' + string(doc.id)", "queue": false, "when": "doc.weight > 0"}
```

Inline, `results[i]` is the mapped result for later effects (`{"type": "set_fields", "values": {"tracking":
"results[0].tracking"}}`) and a refusal refuses the run; `queue: true` calls after the run commits, retried while the
service says "later", and gives `{queued: job}`. Without `key`, the run's own: the same press on the same record version
calls once. Through the gate: `ferrum.call` with `{connection, operation, inputs}` and an effect key (administrators,
and configuration).

**Being called**: `inbound` on the description.

```json
"inbound": {"verify": {"type": "signature", "header": "x-signature", "prefix": "sha256=", "secret": "hook",
                       "timestamp_header": "x-timestamp", "window_secs": 300},
            "delivery_header": "x-delivery-id",
            "map": {"entity": "Alert", "items_at": "/alerts", "by": {"external_id": "item.id"},
                    "set": {"title": "item.summary", "state": "item.status"}, "create": {"source": "'monitor'"}}}
```

`verify` may instead be `{type: token, header, secret}`, or a signature without `timestamp_header` (over the raw
body), saved only with `delivery_header` or `delivery_at`: then a delivery id or a body taken once is refused again
([BD-49](builder/decisions.md#bd-49-inbound-without-a-timestamp)). `delivery_at` (a pointer) names the caller's id when it is in
the body; without either, the digest of what was signed stands for it. `handler: "<app>"` (an app in `apps`, registered
with `HookRegistry::add_inbound_handler`) takes the payload in place of `map`. Bodies up to `max_bytes` (256 KB, at most
1 MB), JSON or a form.

**Pulling**: `"pulls": [{"name": "orders", "operation": "list_orders", "every_minutes": 15, "items_at": "/data",
"cursor": "token", "next_at": "/next", "cursor_input": "cursor", "max_pages": 5, "map": {…}}]` (`cursor: "time"` gives
the moment the last run began). `next_at` is a pick (below): `{"header": "x-next-cursor"}`, or `{"link": "next"}`, whose
address is asked as it is for the next page. On a connection each person signs in to (`owner: person`), a pull runs
as each person connected, with their sign-in and rights; it stops when their sign-in needs reconnecting (until they
connect again) or their account is closed. `GET …/pulls` gives each row's `user` (null: the workspace's).

**Picks** ([BD-46](builder/decisions.md#bd-46-answers-beyond-a-json-body)): a `result` value, a mapping's value and
`next_at` may be a pointer (a key not there reads as null) or `{"at": "/a"}`, `{"header": "Location"}`, `{"link":
"next"}`, `{"expr": "answer.counts.likes + answer.counts.shares"}` (over `answer`, `headers`, `inputs`, `connection`;
in a mapping over `payload`, `item`, `connection`), each with `"required": true` to fail the call or the item when it
is null. In a mapping a plain string that is not a pointer is an expression, as before. From Rust, an object value in
`Operation.result`, `form` or `body` is `connections::spec(&json!({…}))`. A temporary answer's `Retry-After` sets when
a queued call or a pull's retry comes back (capped at an hour).

**Files** ([BD-47](builder/decisions.md#bd-47-bodies-beyond-json)): `"form": {"caption": "{{ inputs.text }}", "media":
{"file": "{{ inputs.photo }}", "max_bytes": 5000000}}` sends `multipart/form-data`; `"body": {"file": "{{ inputs.video
}}"}` sends the file as the body with its type. The caller must be able to download the file; 10 MB a file unless
said, 25 MB at most, 25 MB a request. `"result": {"label": {"file": {"from": "/label_url", "name": "label-{{ inputs.ref
}}.pdf", "max_bytes": 2000000}}}` fetches the address the answer gives (the connection's hosts, no credentials), or
without `from` keeps the answer itself, stores it as an upload attached to the call's record and gives its id.

**Sessions** ([BD-48](builder/decisions.md#bd-48-sign-in-that-is-not-the-authorisation-code-flow)): `"auth": {"type":
"session", "login": "create_session", "refresh": "refresh_session", "token_at": "/accessJwt", "refresh_at":
"/refreshJwt", "expires_at": "/expiresIn", "header": "authorization", "prefix": "Bearer "}`, the two operations
described like any other (`create_session`'s body from `secret.handle` and `secret.app_password`, `refresh_session`'s
header `Bearer {{ inputs.refresh_token }}`); they are never called as operations. A refused login or refresh marks it
needing reconnecting and tells the administrators once. On `auth: oauth`, `provider.dpop: true` binds tokens to the
connection's own key (DPoP), and `provider.registration_url` without `client_id` registers the platform's client once.

**Operating**: `meta` on a description is an object (16 KB at most) the platform keeps and gives back without reading.
A mapping's `"outside": true` marks what it writes as fetched from outside: the change's events carry `origin:
"outside"`, automations read `origin` (empty otherwise), hooks `ctx.origin`; an app's effect for new records that
reaches people checks it ([BD-50](builder/decisions.md#bd-50-operating-a-connection)).

**Signing in to an account**: `"auth": {"type": "oauth", "owner": "workspace" | "person", "provider": {…}}`, the
redirect address registered at the provider being `{PUBLIC_URL}/api/connections/callback`. Two worked descriptions, with
placeholder addresses:

```json
{"authorize_url": "https://login.mailhost.example/oauth2/authorize", "token_url": "https://login.mailhost.example/oauth2/token",
 "revoke_url": "https://login.mailhost.example/oauth2/revoke", "scopes": ["openid", "email", "mail.read", "mail.send", "offline_access"],
 "client_id": "0f3c…", "client_secret": "client_secret", "client_auth": "body", "params": {"prompt": "consent"}, "account_at": "/id_token/email"}
```

```json
{"authorize_url": "https://social.example/oauth/authorize", "token_url": "https://api.social.example/oauth/token",
 "scopes": ["posts.write", "profile.read"], "client_id": "ferrum-acme", "client_secret": "client_secret", "client_auth": "basic",
 "account_at": "/account/handle"}
```

The first is a person's mailbox (`owner: person`, `roles: ["Sales"]`): each salesperson connects their own and every call
made as them uses theirs. The second is the organisation's page (`owner: workspace`), connected once by an administrator.

**Packs and packages** carry `connections: [{name, label, description}]`, never a secret or a token; they arrive turned
off.

### Steps, waits, loops and the clock

**Steps** ([BD-53](builder/decisions.md#bd-53-an-operation-of-steps)): an operation may be several requests in order,
each another operation of the connection; it then sends nothing itself. A step's `inputs` are expressions (the
templates' language without braces) over `inputs`, `steps` (the earlier steps' results by name) and `key`; `when`
makes it only when true. A register-upload, put-bytes, post shape:

```json
"publish": {"inputs": ["owner", "text", "file"], "result": {"id": "/post/id"}, "steps": [
  {"name": "register", "operation": "register_upload", "inputs": {"owner": "inputs.owner"}},
  {"name": "upload", "operation": "put_bytes", "inputs": {"url": "steps.register.upload_url", "file": "inputs.file"}},
  {"name": "post", "operation": "create_post", "inputs": {"text": "inputs.text", "asset": "steps.register.asset"},
   "confirm": {"operation": "find_post", "inputs": {"asset": "sent.asset"}, "proves": "result.id"}}]},
"put_bytes": {"method": "PUT", "url": "{{ inputs.url }}", "body": {"file": "{{ inputs.file }}"}}
```

The operation's `result` reads `{<step>: <its result>}` (none: all of it). Under a key each step is a call of its own
under `<key>:<step>`, its answer kept with the call (masked): the same key again resumes after the last step that
finished, and other inputs under it are a 409. A step whose answer was lost (no answer, a 5xx but 503, an answer that
could not be read) may have happened: it is sent again only when its operation has an `idempotency_header`, or after
its `confirm` (a reading operation; `sent` is the step's own inputs) does not prove it; otherwise the call fails
saying so. One worker makes a key at a time: another gets `RetryAfter` (5 s). An operation may give a whole address
in `url` (a template; one an earlier answer gave), held to the connection's hosts like any other; `credentials: false`
sends it without the connection's credentials (an address signed already); `form_fields` (a template giving an
object, or a list of `{name, value}`) adds a signed upload form's fields before `form`'s.

**Waiting** ([BD-54](builder/decisions.md#bd-54-a-step-that-waits)): a step on a reading operation may wait until its
answer says it is ready: `"wait": {"until": "result.status == 'FINISHED'", "fail_if": "result.status in ['ERROR',
'EXPIRED']", "every_secs": 5, "within_secs": 600, "attempts": 100}` (expressions over `result`, `answer`, `headers` and
the step's names). It fails when `fail_if` holds, after `attempts` asks or `within_secs` after the key's first attempt.
A call waits in place 20 s at most, then answers "later" after `every_secs` (`RetryAfter`); the same key again carries
on from the wait.

**Loops** ([BD-55](builder/decisions.md#bd-55-loops-over-items-and-chunks)): `"for_each": "inputs.texts"` makes a step for
each item, in order, each under `<key>:<step>:<index>`, binding `item`, `index`, `previous` (the result before) and
`results`; the step's result is the list (at most 100 items). With `"range": {"file", "offset", "size"}` (expressions)
each item sends that range of a stored file (parts the service names, each with its own address). `"chunks": {"file":
"inputs.video", "bytes": 8388608, "remainder": "own" | "last", "offset_from": {"operation": "probe", "inputs": {…}},
"restart_from": "init"}` sends a stored file a chunk at a time, read from storage a range at a time, binding `chunk:
{index, offset, end, size, total, count, last}`, each chunk under `<key>:<step>@<offset>`; the result is the last
chunk's. A chunked upload of a resumable session:

```json
{"name": "upload", "operation": "put_chunk", "chunks": {"file": "inputs.file", "bytes": 8388608,
   "offset_from": {"operation": "probe", "inputs": {"url": "steps.start.url", "total": "chunk.total"}}},
 "inputs": {"url": "steps.start.url", "file": "inputs.file",
            "range": "'bytes ' ~ chunk.offset ~ '-' ~ chunk.end ~ '/' ~ chunk.total"}}
```

with `put_chunk` a `PUT` to `{{ inputs.url }}` with `content-range: {{ inputs.range }}` and `"success": [308]` (statuses
that also mean it worked). A chunk whose answer was lost goes on from what `offset_from`'s `offset` result says the
service holds (a number, or `bytes=0-<last>`; none with a 2xx: the upload is complete, that answer the result), or
starts the upload again from the step `restart_from` names under new keys (at most twice), or stops, saying so. Files
sent in chunks may be up to `FERRUM_CONNECTION_UPLOAD_MB` (25 MB unless the operator says); a chunk is at most 25 MB.

### Moving an app's own HTTP client onto a connection

Social, ERP's carriers and the service apps each build requests themselves. To move one:

1. Write what the app calls as a connection description: `base`, `auth`, one operation per call, `result` for what the
   app reads back, `error_at`, `temporary` and `idempotency_header` where the app had its own. Ship it in the package
   (`connections:`) so a workspace sees it offered; keep the app's settings for what is the app's.
2. Move the app's stored API keys to the connection's secrets: an upgrade step reads the app's sealed value and saves
   the connection with it (`save_connection_def`), then clears the app's copy. Names in templates stay `secret.<name>`.
3. Replace the client code with `connections::call(k, ctx, Some("<app>"), &Call { … })`, the key the one the app sent as
   its idempotency key, `entity`/`record` the record it is about. `Unavailable` is the app's "try later" (or queue it with
   the `call` effect), `Invalid` its "the service said no".
4. Move the app's webhook route to the connection's inbound address: a `map` when it only writes records, else
   `handler` with the app's code as an `InboundHandler`. Keep the old route answering until every caller is pointed at
   the new address, then delete it and its signature code.
5. Where a person signs in (Social's networks, a mailbox), describe the provider under `auth: oauth` and drop the app's
   token table after asking each person to connect again; the tokens are never moved by copying.
6. Delete the app's fixtures in favour of a test connection's, and its SSRF checks: the platform's guard applies.

## Messages

Messages to people outside and inside the workspace come from templates a workspace edits, on e-mail, text message,
chat app or in-app notice, every send recorded ([BD-20](builder/decisions.md#bd-20-message-templates-and-channels)).

**A package's defaults** (`package.yaml`): one per message the package sends, key `{package}.{name}`.

```yaml
messages:
  - key: bookings.confirmed
    label: Booking confirmed
    entity: Booking            # optional: the record it is about
    category: operational      # or promotional (opt-outs, quiet hours)
    channels: [email, text]    # optional: the order tried; else email, text, chat, inapp as it has bodies
    values: [link]             # what the sender passes, read as values.link
    languages:
      en:
        email: {subject: "Booking {{ doc.number }} confirmed", body: "<p>See you on {{ doc.starts_on }}. <a href=\"{{ values.link }}\">Change it</a></p>"}
        text: {text: "Booking {{ doc.number }} confirmed for {{ doc.starts_on }}: {{ values.link }}"}
        chat: {template: booking_confirmed, values: ["{{ doc.number }}", "{{ doc.starts_on }}"]}   # or {text: …}
        inapp: {title: "Booking {{ doc.number }} confirmed", text: "{{ doc.service }}"}
      de:
        text: {text: "Buchung {{ doc.number }} bestätigt"}
```

Bodies are MiniJinja (print formats' engine) over `doc` (the record as the workspace holds it, secret fields left
out), `settings.<Entity>.<field>`, `values` and `opt_out` (a promotional message's signed opt-out link). An e-mail's
body is HTML, every value it prints escaped; everything else is text.

**A workspace's templates** (administrators):

* `GET /api/admin/messages/templates` → `{templates: [{key, label, entity, category, channels, languages, source, default_changed}]}`;
  `source` is `package`, `edited` (the workspace's copy of a default) or `workspace` (its own).
* `GET /api/admin/messages/templates/{key}` → `{template, default, edited, default_changed}`.
* `POST /api/admin/messages/templates` with a template (the YAML entry above as JSON) saves it: a copy of a package's
  default, or the workspace's own under a key no installed package's name starts. Refused (422 on `template`, `key`
  or `entity`) when a body does not compile or fails against a record with every field, reads a field the entity does
  not have (a secret field included), or a value it does not declare, said in plain words after the language and
  channel the screen places it by (`en, text: doc.titel is not a field of Note (line 2)`, `en, email: the body reads
  values.name, which is not among the values this template says the sender passes: add name to its values`);
  document templates say the same after their language.
* `DELETE /api/admin/messages/templates/{key}` goes back to the default (or removes the workspace's own).
* `POST /api/admin/messages/preview {template, channel, language?, record_id?}`: `template` a key or an unsaved
  template; against the record named (read with the caller's rights), else the latest the caller may read →
  `{language, message, record_id}`, `message` being `{subject, html}`, `{text}`, `{template_name, values, text}` or
  `{title, text}`. Sends and writes nothing.
* `POST /api/admin/messages/test {template, channel, phone?, record_id?}` sends it to the caller themselves.

**The effect**, on buttons, transitions, approvals and automations, with `when` like every effect:

```json
{"type": "message", "template": "bookings.confirmed", "to": ["field:customer", "role:Front desk"],
 "channels": ["text", "email"], "all": false, "values": {"link": "'https://…/' + doc.token"}}
```

`to` is one spec or a list: `owner`, `user:<id>`, `role:<name>`, `field:<name>` (an e-mail or phone field, or a link to
a record whose first e-mail and phone fields are used: a customer, a contact, an employee),
`links:<link>.<table>.<field>` (that field of each line of a table of the record the link field names: the members of an
assignment group; read as the workspace, as `links` is; at most 100 lines, more refuses the send: `links:team.members.email:
101 lines, more than the 100 one message goes to; nothing was sent`; checked when saved) or `expr:<CEL>` giving an
address, a number or `{email, phone, language, time_zone}`. The same `links:` spec is a `notify` recipient (the users
whose e-mail is there, each told once) and an `email` action's `to` (one mail to each address). It gives the sends. An approval policy's `message` names a
template its notices go out with instead of the fixed wording (`values.notice`: `waiting`, `reminder`, `escalated`,
`approved`, `rejected`, `not_done`; `values.title`: the fixed wording).

**From an app**, in its own transaction (nothing goes unless it commits):

```rust
pub async fn ferrum_kernel::messages::send(k: &Kernel, tx: &mut Tx, ctx: &Ctx, m: &Message) -> Result<Vec<Sent>>;
// Message { template, entity, record, to: Vec<Recipient>, channels, all, values, attachments }
// Recipient { user, email, phone, language, time_zone }; Recipient::user(id), Recipient::address("a@b" | "+49…")
// messages::recipients(k, tx, ctx, entity, doc, "field:customer") resolves a spec as the effect does.
pub async fn Kernel::send_code(&self, tx, ctx, purpose: &str, template: &str, to: &Recipient, channel: &str) -> Result<Sent>;
pub async fn Kernel::verify_code(&self, tx, ctx, purpose: &str, to: &Recipient, code: &str) -> Result<bool>;
```

For each recipient the channels are tried in order; the first they can be reached on is used (each, with `all`).
One that cannot be used is recorded `skipped` with why: no address, no phone number, a number without a calling code,
opted out, no channel configured, a limit. A code is six digits, kept as a salted digest, valid ten minutes, once,
within five tries; five a recipient an hour. The template reads it as `values.code`.

**The log**: `GET /api/messages/log?entity=&record_id=` (whoever may read the record) or `?recipient=` (administrators;
an address or a number) → `{sends: [{id, template, channel, recipient, user_id, entity, record_id, language, state,
reason, provider_id, error, content, created_at, updated_at}]}`, newest first. `state`: `queued`, `held` (quiet hours),
`sent`, `delivered`, `failed`, `skipped`. `content` is what the test channel delivered, a code masked.

**Opt-outs**: `PUT /api/admin/messages/opt-outs {address, channel, opted_out}` (staff); `GET
/api/admin/messages/opt-outs?q=&channel=&limit=&offset=` (administrators) → `{opt_outs: [{address, channel, source,
recorded_at, recorded_by, recorded_by_name}], total}`, newest first, `q` part of an address or number, `limit` 1 to 200
(50); erasing a person keeps their opt-outs without the address (a keyed digest still stops promotional messages to
it; listed with `address: null` and `erased` naming the row, never found by `q`; `PUT` with that row as `address` and
`opted_out: false` removes one), takes the consents they gave, and their report lists both before; a promotional message's
`opt_out` link (`GET` shows a page, `POST /api/messages/opt-out?token=` records it); a reply with a stop word. Kept
per address or number and channel with the consent records. Operational messages always go.

**Settings** (`PUT /api/admin/settings`, `messages`): `calling_code` (`+49`) completes a number written without `+`,
`trunk_prefix` (`0`) is dropped from it first; with no calling code such a number is refused for sending, never
guessed. `quiet_hours` `{from: "21:00", to: "08:00"}` hold promotional text and chat-app messages (none by default),
in the recipient's zone when known, else the workspace's. `per_hour` (500) and `per_recipient_hour` (10) limit text and
chat-app messages.

**Channel connections** (administrators): `GET|POST /api/admin/messages/channels`, `PUT|DELETE
/api/admin/messages/channels/{id}` with `{kind: text | chat, name, description, secrets?, enabled?}`. One enabled for
each kind; `secrets` (`{name: value}`) are sealed with the server's key and never read back (left out, they stay).
`description`: `url`, `method` (POST), `headers`, `body` (a JSON template) or `form` (fields), each a template over
`to` (international form), `text`, `template_name`, `values`, `language`, `from` and `secret.<name>` (`| json` writes a
value as JSON); `id_at` and `error_at` (JSON pointers into the answer); `temporary` (statuses meaning "later";
408, 425, 429, 500, 502, 503, 504 when left out; any other non-2xx fails at once). `{"test": true}` is a test channel:
it delivers into the log only. Two worked examples, with placeholder addresses:

```json
{"kind": "text", "name": "SMS gateway", "secrets": {"key": "…", "hook": "…"},
 "description": {"url": "https://sms.gateway.example/v1/send", "form": {"to": "{{ to }}", "message": "{{ text }}", "sender": "{{ from }}"},
   "headers": {"Authorization": "Bearer {{ secret.key }}"}, "from": "Acme", "id_at": "/message_id", "error_at": "/error",
   "inbound": {"auth": "token", "secret": "hook", "id_at": "/message_id", "status_at": "/status",
     "statuses": {"DELIVRD": "delivered", "UNDELIV": "failed", "REJECTD": "failed"}, "from_at": "/from", "text_at": "/text",
     "stop": ["STOP", "UNSUBSCRIBE"], "start": ["START"]}}}
```

```json
{"kind": "chat", "name": "Chat app", "secrets": {"token": "…", "hook": "…"},
 "description": {"url": "https://chat.provider.example/v1/accounts/123/messages",
   "headers": {"Authorization": "Bearer {{ secret.token }}"},
   "body": "{\"to\": {{ to|json }}, \"type\": \"template\", \"template\": {\"name\": {{ template_name|json }}, \"language\": {{ language|json }}, \"parameters\": {{ values|json }}}}",
   "id_at": "/messages/0/id", "error_at": "/error/message",
   "inbound": {"auth": "hmac", "header": "X-Signature", "secret": "hook", "id_at": "/status/id", "status_at": "/status/state",
     "statuses": {"sent": "sent", "delivered": "delivered", "read": "delivered", "failed": "failed"},
     "from_at": "/message/from", "text_at": "/message/text", "stop": ["STOP"], "start": ["START"]}}}
```

**Delivery reports and replies**: `POST /api/messages/inbound/{channel}` (no session; the address the channel listing
gives). Authenticated as `inbound` says: `hmac` (the body's HMAC-SHA256 under `secret.<inbound.secret>`, in hex, in
`header`, `sha256=` before it or not) or `token` (`header`, or the `token` query parameter, equal to the secret). JSON
or a form. A report (`id_at` and `status_at` present, the status mapped by `statuses`) moves the send forward only
(`sent`, then `delivered`; `failed` is final); a reply (`from_at`, `text_at`) with a `stop` or `start` word records the
number's choice for that channel. Unknown, unsigned or wrongly signed calls are refused (404, 401) and counted (the
channel's `refused`).

### Moving an app's fixed wording to templates

1. For each message the app sends, add a `messages:` entry to its `package.yaml`: key `{package}.{name}`, the entity
   it is about, `category` (operational unless it markets), the bodies it sends today in `en` (its Rust subject and
   HTML become `email.subject` and `email.body`, `{{ doc.<field> }}` where it formatted a field), and `values` for
   whatever it computes that is not a field (a link, a code, a time in the guest's zone).
2. Where it built the mail and called `queue_mail` / `queue_mail_with` / `notify::send_kind`, call
   `ferrum_kernel::messages::send` with `Message { template, entity, record, to, values, attachments, ..Default::default() }`
   in the same transaction, `to` as `Recipient`s (`Recipient::address(&email)`, `Recipient::user(id)`; a phone when the
   record has one, so the workspace may add a text body). Leave `channels` empty: the template's order decides.
3. A one-time code: `Kernel::send_code` and `Kernel::verify_code` in place of the app's own code table.
4. Delete the Rust wording, and run the app's tests against the rendered message: a preview (`preview_message`) of
   each template against a fixture record gives the same subject and text as before.
5. Say in the app's README that its messages are templates a workspace edits under Settings, and which `values` each
   reads.

## Documents

Templates that render a record into a page, text, CSV or XML (`crates/kernel/src/documents.rs`, decision
[BD-21](builder/decisions.md)): letters, statements, receipts, labels, bank and filing files.

**A template.** A package declares its defaults in its `package.yaml`; a workspace edits a copy, sees the default
beside it, resets it, and is told (`default_changed`) when a release changed the default it edited, as with messages.
A workspace's own template has a key no package's name starts.

```yaml
documents:
  - key: hr.offer_letter          # {package}.{name}
    label: Offer letter
    entity: Offer                 # the record it renders
    output: page                  # page (HTML, a PDF where the server makes PDFs) | text | csv | xml
    default: true                 # what the entity prints with when none is picked (a workspace's wins)
    roles: [HR Manager]           # who may pick it; none: whoever may print the record
    values: [joining_bonus]       # what a caller passes, read as values.<name>
    languages:
      en:
        body: "<h1>{{ settings.Company.company_name }}</h1><p>Dear {{ doc.candidate }}, … {{ doc.ctc|amount }} ({{ doc.ctc|words }})</p>"
        footer: "Page {{ page_number() }} of {{ page_count() }}"
      hi: {body: "…"}
  - key: payroll.bank_upload
    entity: Payroll Run
    output: text                  # text, csv and xml also take:
    encoding: iso-8859-1          # utf-8 (default) | iso-8859-1 | ascii; a character it cannot hold is refused
    crlf: true                    # every line ends CR LF (else LF); the last line ends as the template's does
    languages:
      en: {body: "{% for l in doc.lines %}{{ l.account|pad(18) }}{{ l.net|fixed(2)|pad(15, 'right', '0') }}\n{% endfor %}T{{ doc.lines|length|pad(6, 'right', '0') }}{{ doc.lines|total('net') }}\n"}
```

A body reads `doc` (the record, its lines, `_titles` of its links), `links.<field>` (the record a link field names),
`settings.<Entity>`, `values`, `entity` (its fields' labels), `language`, `currency` (the record's `currency`),
`currencies` (the workspace's currency records), `title` (the record's) and `now`. Each output escapes what it prints for
its own syntax: a page as HTML; CSV quotes a value holding a separator (`,` `;` tab), a quote or a line break, and
keeps one a spreadsheet would run as a formula as text; XML writes markup characters as entities and leaves out what
XML cannot hold; text prints as written (use `pad`, `csv`, `xml`). Nothing prints as `none`. A save renders every
body against a record with every field (one line in each table, a record of each entity it links to) and refuses an
unknown field, a secret one, an undeclared value, or a body that does not compile (422 on `template`). Bodies are at
most 256 KB.

**Helpers**, each in the document's `language` and `currency` unless given one:

| Helper | Gives | Example |
| --- | --- | --- |
| `number(decimals?, language?)` | a number grouped and marked as the language writes it (lakhs for `hi`, `en-IN`; `1.234,5` for `de`; Arabic-Indic digits for `ar`) | `{{ 1234567.5\|number(2) }}` → `12,34,567.50` in `hi` |
| `amount(currency?, show?, language?)` | an amount with the currency's decimals (its record's `precision`, else ISO's) and its symbol (`show`: `symbol`, `code`, `none`) | `{{ doc.total\|amount }}` → `₹12,34,567.50` |
| `words(currency?, language?, names?)` | an amount in words in `en`, `en-IN`, `hi` or `ar`, the units named from the platform's table (`names`: `[unit, units]` or `[[unit, units], [fraction, fractions]]` for one it lacks); another language is refused | `{{ doc.total\|words }}` → `twelve lakh … rupees and fifty paise` |
| `date(style?, language?)` | `short`, `long` (the month's name in the language) or a `strftime` pattern | `{{ doc.posting_date\|date('long') }}` → `2 नवंबर 2026` |
| `fixed(places?)` | a number as data files write it: a point, no grouping | `{{ l.net\|fixed(2) }}` → `1234567.50` |
| `total(attribute?)` | the exact sum of a list or of one attribute of it | `{{ doc.lines\|total('net') }}` |
| `pad(width, align?, fill?)` | exactly `width` characters, cut or filled (`right` fills on the left); a line break becomes a space | `{{ l.amount\|pad(12, 'right', '0') }}` |
| `csv` | one CSV field (escaped as HTML on a page) | `{{ doc.name\|csv }}` |
| `xml` | text or an attribute's value in XML | `<Nm>{{ doc.name\|xml }}</Nm>` |
| `barcode(value, kind?, module=, height=, text=)` | Code 128 (`code128`, the default) or EAN-13 (`ean13`, 12 digits and its check digit added, or 13 checked) as inline SVG; a wrong check digit or a character it cannot hold is refused | `{{ barcode(doc.item_code) }}` |
| `qr(text)` | a QR code as inline SVG | `{{ qr(doc.irn_qr) }}` |
| `page_number()`, `page_count()` | in a page's `header` and `footer`: this page and how many, filled in by the PDF service | `Page {{ page_number() }} of {{ page_count() }}` |

The print formats (`set_print_template`) and message bodies have these helpers too, in English and naming the currency unless told otherwise.

**Pages and PDFs.** A page is HTML; where the server has a PDF service (`FERRUM_PDF_URL`, Gotenberg's Chromium route)
it is also a PDF: the service lays it out, its `header` and `footer` repeated on every page in margins made room for, a
page size or margins from the body's own CSS (`@page {size: 100mm 50mm; margin: 2mm}`). Without the service a page
is HTML, its header and footer left out.

**What it shows.** A person printing gets the record as they may read it (a field group they may not read is not
there, as on a print). A document generated by the system (the `generate` effect, `generate_document`) is rendered as
the workspace holds the record, its secret fields left out: it goes to people outside, so who caused it is not who
reads it.

**Administrators**: `GET /api/admin/documents/templates` → `{templates: [{key, label, entity, output, default, roles,
languages, source: package|workspace|edited, default_changed}]}`; `GET /api/admin/documents/templates/{key}` →
`{template, default, edited, default_changed}`; `POST /api/admin/documents/templates` (a template) → `204`, refused
with a 422 on `template`, `entity` or `key`; `DELETE /api/admin/documents/templates/{key}` → `204` (back to the default;
a workspace's own one is removed); `POST /api/admin/documents/preview {template: key | a template not saved, language?,
record_id?}` → `{language, output, content, header, footer, record_id}` (the record as the administrator reads it, else
the latest; writes nothing). Packs carry the workspace's own and edited templates (`documents`).

**People printing**: `GET /api/documents/choices/{Entity}` → `{templates: [{key, label, output}], default}` (those the
caller's roles may pick; `default` null when the entity prints with its print format); `GET
/api/documents/{key}/render/{id}?language=&format=pdf|html` → the document (a page as a PDF where the server makes PDFs
unless `html` is asked for; a file as an attachment with its charset); `POST /api/documents/{key}/labels {ids, copies?,
language?, format?}` → one document for many records (items, batches, bins, parcels): each record `copies` times, a
page's each starting a page (write a label's body as a fragment with its own `@page` size), a file's one after another;
1 to 200 records, 100 copies, 1000 in all a call, more refused (ask for the next ones). `GET /api/v1/{E}/{id}/print`
takes `?template=key` (one of the choices) and `language`, and prints with the entity's default template when it has
one; otherwise it is the print format as before. A page needs read permission; a text, CSV or XML file needs export
permission and each download is on the record's audit trail (`document:export`, with the file's digest).

**Generating and keeping.** The `generate` effect (configured buttons, transitions, approvals, automations):

```json
{"type": "generate", "template": "hr.offer_letter", "name": "'Offer ' + doc.candidate", "language": "doc.language",
 "replace": true, "values": {"joining_bonus": "doc.bonus"}}
```

`name`, `language` and `values` are expressions. It renders the template for the record, as a PDF where the server
makes PDFs (HTML otherwise), stores it attached to the record in the run's transaction (gone if the run rolls back;
the bytes are written first, so a rolled-back run leaves an object no row names), and with `replace` drops the file
this template made for the record before, except one a queued mail still waits to attach, which stays on the record
as an ordinary file. It gives `{file, url, name, content_type, size, sha256, template, entity, record}`, which later
effects read as `results[n]`: a `message` attaches it with `"attachments": ["results[0]"]` (each an id or what
`generate` gave, a file of the same record only; another's is refused). Apps call `ferrum_kernel::documents::generate(k,
tx, ctx, &Generate { template, record, name, language, replace, values })` in their own transaction, or
`Kernel::generate_document(ctx, &Generate)` outside one; the caller must be able to read the record.

**A report as a document.** A template with `query: {"report": "<a saved report's label>", "group_by"?: "<a column
that links to a record>"}` (its `entity` the report's; no record's default) renders the report's rows: bodies read
`rows`, `columns`, `group` (the record of the group, its secret fields left out; null without `group_by`), `values`,
`settings`, `language`, `currencies` and `now`, and compute their totals (`rows|total('amount')`). Checked when saved
against the report's columns (it is run once to learn them) and the grouped entity. `POST /api/documents/{key}/query
{params?: {filters, from, to}, values?, language?}` → one file for the whole result (as `render` serves one; a file of
data needs export permission and is on the audit trail), or with `group_by` `{files: [what generate gives], next:
null}`, a file kept on each record (the template's earlier one there replaced). The rows are read as the caller may
(the report path's rights); more than 10,000 rows, or rows for more than 100 records, is refused with the numbers. The
`report_documents` effect makes the grouped files as the workspace and, with `message`, sends each record its own:

```json
{"type": "report_documents", "template": "finance.statement", "params": {"from": "2026-10-01", "to": "2026-10-31"},
 "message": {"template": "finance.statement_mail", "to": "field:email", "channels": ["email"]}}
```

It queues a job (`platform.document.groups`) in the run's transaction and gives `{job}`; each job does 100 records and
queues the next after the last record it did, so a run over more is done in turns, never cut. `message.to` takes the
`message` effect's specs against each record (no `expr:`); the message template is about the grouped entity. Apps call
`Kernel::render_query_document` / `generate_query_documents` with a `QueryRun {template, params, values, language}`.

**Handing a document to signing.** What `generate` gives is what the Sign app takes: an `invoke` effect after it
passes `results[0].file` (the stored file) and `results[0].sha256` (its digest, for the signing request to check the
file it was given is the one generated), with the record's entity and id, to the Sign app's capability. That
capability's input is the Sign app's to declare; nothing here signs.

### Print formats

A page template is a **print format** (decision [BD-28](builder/decisions.md#bd-28-print-formats); the full contract is
`tools/PRINT-FORMATS-SPEC.md` in the interface work): an administrator designs how an invoice or an order prints, a
person printing picks a format, a letter head, a language and copies. A format is a `template` (MiniJinja bodies, as
above) or a `layout` (sections, columns and blocks rendered by the platform's own renderer,
`crates/kernel/src/layout.rs`; no tenant markup runs). Every field below is optional, today's behaviour the default:

```yaml
documents:
  - key: selling.sales_invoice
    label: Sales Invoice
    entity: SalesInvoice
    default: true
    layout_file: print/sales_invoice.layout.json   # a package's only; read into `layout` as it loads
    page: {size: A4, orientation: portrait, margin_mm: {top: 15, right: 12, bottom: 15, left: 12}}   # A3 A4 A5 Letter Legal Custom (+width_mm, height_mm)
    letterhead: default          # default (the record's company's, else Print Settings') | none | a LetterHead's id
    copies: [Original, Duplicate] # labels offered when printing (≤ 5); none: Print Settings'
    enabled: true                 # a disabled format is neither offered nor printed
    based_on: selling.sales_invoice  # set by duplicate
```

A layout is `{version: 1, style?, watermark?, sections: [{id, title?, columns: [{id, blocks: [...]}], widths?, gap_mm?,
border?, background?, padding_mm?, keep_together?, break_before?, margin_top_mm?, when?}]}`; blocks are `field`,
`heading`, `text`, `image`, `table`, `totals`, `signature`, `barcode` (`code128`, `ean13`, `qr`), `copy_label`,
`divider`, `spacer`, `page_break`, each with `id`, `when` (CEL over `doc`, `links`, `company`, `settings`, `print`),
`align`, `margin_top_mm`. Paths: `doc.<field>`, `links.<link field>.<field>`, `company.<field>` (the record's company,
read as the workspace), `settings.<Entity>.<field>`, `row.<field>` (a table's columns), `print.copy|now|language|title|
entity|time|letterhead|letterhead_name` (`time`: when it was printed, in the workspace's time zone; `letterhead`:
whether the print wears a letter head after every choice is applied, so a format prints its own company block with
`when: "!print.letterhead"`; `letterhead_name`: its name; `print.page`/`print.pages` in a letter head:
filled in by the PDF service where it repeats the letter head, and a paragraph or heading that names them is left out
elsewhere, so a footer text `[{t: "Page "}, {v: "print.page"}, {t: " of "}, {v: "print.pages"}]` prints "Page 2 of 3"
in a PDF and nothing in the HTML). A `table` prints `doc.<table>`, `links.<link field>.<table>` (read as the person may)
or `company.<table>` (read as the workspace); a column may say `hide_zero: true` (a zero cell is blank), and its
`secondary` line prints only when it differs from the cell. Every value a layout prints is isolated from the line around it (`<bdi dir="ltr">` for numbers, amounts and text
with no right-to-left letters, `<bdi dir="auto">` otherwise), so a phone number or an amount keeps its order in an
Arabic page; in a template write `{{ doc.phone|bdi }}` for the same. A date prints as `29 Oct 2026` (`medium`, the month's name in the print's language) unless a format says `date:
"short"` (the language's numeric order) or `"long"`. A format may say `sign: "abs"` (a number, amount or amount in
words without its sign: a credit note's totals print positive) or `sign: {abs_when: "<condition>"}` (without it when the
condition holds, e.g. `doc.is_return`). Unknown keys, paths, secret fields, colours other than `#rrggbb`,
numbers out of range and every limit (256 KB, 100 sections, 4 columns, 50 blocks a column, 500 in all, 20 table columns,
100 conditions of ≤ 500 characters, 20 images) are refused when saved (`422`, `field: "layout.<id>"`) and again when it
renders. A template's error names where it is: `"en body, line 3: …"`.

**Letter heads and settings** are records of the core: `LetterHead` (`letter_head_name`, `company`, `header` and
`footer` as `{"sections": [...]}` of `field`, `text`, `heading`, `image`, `divider`, `spacer` over `company.*`,
`settings.*`, `print.*`, their heights (empty: as tall as their blocks come out on each print, worked out from the
blocks that print, their font sizes, wrapped text and declared image heights), `disabled`; a save checks them, errors on `header`/`footer` as `"<block id>:
…"`), `PrintSettings` (paper, orientation, margins, font, `letter_head`, `repeat_header`, `repeat_footer`,
`allow_print_draft`, `allow_print_cancelled`, `draft_watermark`, `cancelled_watermark`, `copies` one per line) and
`Company.letter_head`. The letter head a page wears: the one asked for, else the format's, else the record's company's
(its first link to Company), else Print Settings', else none; a disabled one is none. In a PDF with `repeat_header` /
`repeat_footer` the PDF service repeats it in the page's margin (grown by its height) with page numbers; otherwise it
is in the body, before the first section and after the last.

**What it shows.** The record and its links as the person printing may read them; the company, letter head and
settings as the workspace holds them, never a secret; a document generated for outside (`generate`) as the workspace
holds the record, secrets left out. Images are only this workspace's stored files, inlined as `data:` URLs (the PDF
service fetches nothing): a format's own image (`{file: "/files/<id>"}`), an attach field (`{value: "company.acc_logo"}`;
a record's attachment read as the person may), a letter head's own attachments; png, jpeg, gif, webp or svg by their
bytes, 1 MB each, 4 MB and 20 a page. Templates have `image(value)` (a `data:` URL or nothing), `company`, `print` and
`letterhead.header`/`letterhead.footer` (a template naming `letterhead.` places it itself). A submittable record's
draft prints with Print Settings' draft watermark, a cancelled one with the cancelled one; Print Settings may refuse
printing either (`403`; an administrator's preview still renders).

**People printing**: `GET /api/documents/choices/{Entity}?record=<id>` → `{templates: [{key, label, output, kind,
languages, letterhead, copies, page: {size, orientation}}], standard: {label}, default, letterheads: [{id, name,
company, default_for_record}], languages, copies, may_print, refusal}` (enabled formats the caller may use; `default`
is the workspace's choice, else a workspace template flagged default, else a package's, each only when enabled and
the caller may use it; among packages' defaults, the default of a package that depends, directly or not, on the other's
package wins (an add-on over what it extends), then the first by key; null: the print format, offered as `standard`
with Print Settings' `page`). `GET /api/v1/{E}/{id}/print?template=<key|
standard>&letterhead=<none|id>&language=&copy=<label>&copy=…&format=pdf|html` → the page (HTML with the sandboxing
policy, or `application/pdf` named `<label> <number>.pdf` on the format's paper); `403` roles or a refused draft,
`404` an unknown format, `422` a disabled one or a bad parameter (`fields: [{field: template|letterhead|copy|language}]`; a letter head id that is
not there or not in use is one).
`POST /api/v1/{E}/{id}/email {to, subject?, message?, pdf?, template?, letterhead?, language?, copies?}` attaches
exactly what `/print?format=pdf` makes with the same choices (the default format when none is named); the choices may
also come nested as `print: {template, letterhead, language, copies}`.

**Administrators**: `GET /api/admin/documents/templates?entity=&output=` rows also carry `kind`, `is_default`,
`enabled`, `based_on`, `updated_by`, `updated_by_name`, `updated_at`; saving a template whose `default` is as it was read
changes no default (`PUT …/defaults` chooses; marking one `default: true` that was not makes it the choice); `GET …/templates/{key}` also `etag` (and the `ETag` header);
`POST …/templates` takes `If-Match: "<etag>"` (`409` when someone saved it since; answers `204` with the new `ETag`);
a package's layout is not edited in place (`422` on `key`: duplicate it; its paper, roles, copies and `enabled` change
in an edited copy). `POST …/templates/{key}/duplicate {key, label}` → `201 {template}` (the workspace's own key,
`based_on`, not the default, enabled; `409` when taken). `PUT /api/admin/documents/defaults/{Entity} {key|null}` →
`204` (the workspace's default page format; null removes the choice). `GET …/templates/{key}/versions` → `{versions:
[{n, saved_by, saved_at}]}` (the last 50 saves), `GET …/versions/{n}` → `{template}` (restoring is saving it).
`GET /api/admin/documents/fields/{Entity}` → the designer's catalogue (`fields`, `system`, `tables` with `columns`,
`links` with their `fields`, `company`, `computed`, `print`, `settings`; no secret). `POST /api/admin/documents/preview
{template: key | a template not saved, record_id?, sample?, language?, letterhead?, copies?, mode?: print|design,
format?: html|pdf}` (`letterhead` may also be a letter head not saved yet, `{id?, company?, header, footer,
header_height_mm?, footer_height_mm?}`, checked as a save checks it) → `{language, output, kind, content, header, footer, record_id, warnings, ms}` (a layout's `content`
is the whole page; `mode: design` adds `data-b`/`data-slot` marks and placeholders, and draws what a condition hides on the
record as a collapsed ghost (`<div class="pf-ghost" data-b="<id>" data-ghost="<kind>">`, a table column as
`<th class="pf-ghost-col" data-b="<id>" data-ghost="column">` with no width), or at full size dimmed with `design: "all"`;
`sample` renders a sample record that reads as a document (a number, a customer, three lines that add up, a tax line,
totals in the company's currency, today's dates); the answer also names `latest_record: {id, title}`; `format: pdf` answers the PDF). `GET /api/print/layout.css` is the renderer's
CSS, to read. Saves, resets and default changes are audited (`DocumentTemplate/save|reset|default`). Packs carry the
workspace's defaults (`document_defaults`). `GET /api/version` says `print: 2`. A package that uses any of this
(`layout_file`, `page`, `letterhead`, `copies`, `enabled`, a LetterHead or PrintSettings record in its seed) needs
the app API version 0.3.3: its app says `platform: ">=0.3.3, <0.4"` (an older server refuses the fields it does not
know). The app API is 0.3.6 now: what each version carries is in
[BD-45](builder/decisions.md#bd-45-quorums-from-the-record-absent-parameters-targets-on-every-write-rendering-for-another-transport-and-the-app-api-034)
(0.3.4), [BD-51](builder/decisions.md#bd-51-the-connector-hubs-instances-move-and-the-app-api-035) (0.3.5) and
[BD-52](builder/decisions.md#bd-52-a-link-limited-by-another-field-of-the-record-and-the-app-api-036) (0.3.6).

### Moving an app's print layout or file to a document template

1. Add a `documents:` entry to the app's `package.yaml`: key `{package}.{name}`, the `entity`, `output` (`page` for a
   print layout; `text`, `csv` or `xml` for a file, with its `encoding` and `crlf`), `default: true` on the one its
   entity prints with, `roles` if only some may pick it, and the body in `en` (its `print/<Entity>.html` moved in
   unchanged: the variables a print format reads are all there).
2. Replace what the template computed by hand: `money` stays as it was, but an amount with its currency is
   `amount`, an amount in words `words`, a date in the reader's form `date`; a file's fixed-width fields `pad`, its
   numbers `fixed`, its totals and counts `total` and `length`; escaping a CSV or XML value is the output's own.
3. Where the app wrote a file in Rust (a bank file, a filing), render it from the template instead: `render_document`
   for a person downloading it, `generate` to keep it on the record (and attach it to a mail). Keep the Rust builder
   until a test renders the template against a fixture record and gets the same bytes.
4. Remove the `print_formats` entry once the template is the entity's default (a print without `?template=` then
   uses it); a workspace that customized the print template with `set_print_template` keeps it until it picks a
   template.
5. Say in the app's README which documents it ships, their `values`, and that a workspace edits them under Settings.

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

`POST /api/files` (multipart: `file`, optional `entity`, `record_id`, `public`; ≤25 MB) → `{id, url: "/files/<id>", file_name, size, sha256}`; `GET /files/{id}` (authenticated download, streamed; one `Range: bytes=…` answered with 206). Every download carries its type (a file kept without one as `application/octet-stream`), `X-Content-Type-Options: nosniff` and `Content-Disposition: attachment` with its name; a type a browser would run (HTML, SVG, XML, script) also a sandboxing CSP, also on the public route, where other types are inline. A generated document keeps the type it was made as ([BD-37](builder/decisions.md#bd-37-generated-documents-keep-their-type)).

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
