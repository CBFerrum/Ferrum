# Ferrum HTTP API

Base URL: the server root (dev `http://127.0.0.1:5150`). JSON everywhere. A live, tenant-specific
OpenAPI 3.1 document is at `GET /api/openapi.json` (it includes every customization).

## Conventions

| Topic | Rule |
|---|---|
| Auth | `Authorization: Bearer <session token>` from `/api/auth/login`, or `Bearer fk_<prefix>_<secret>` (API key) |
| Money & decimals | **Strings** (`"1234.5"`), never JSON floats. Send strings too; numbers are accepted but avoid floats |
| Dates | `YYYY-MM-DD`; date-times RFC 3339 UTC (`2026-10-28T10:15:00.000Z`) |
| Links | UUID strings. Add `titles=true` to list/get to receive `_titles: {field: "Display title"}` on records and child rows |
| Concurrency | Records carry `version`; responses send `ETag: "v<version>"`. Send `If-Match: "v<version>"` on update/delete/submit/cancel; a mismatch → `409` |
| Idempotency | `POST /api/v1/{entity}` accepts `Idempotency-Key` (8–200 chars). A replay returns the original record with `200` and `Idempotent-Replayed: true` |
| Errors | `{"error": code, "message": text, "fields"?: [{"field", "message", "row"?: [tableField, rowIndex]}], "error_id"?}`; codes: `validation_failed` (422), `invalid` (422), `not_found` (404), `conflict` (409), `forbidden` (403), `unauthorized` (401), `too_many_requests` (429), `retry`/`unavailable` (503, with `Retry-After`) |
| Document status | `docstatus`: 0 draft, 1 submitted, 2 cancelled. Submitted documents are immutable; cancel + amend to change |

## Auth — `/api/auth`

| Method & path | Body → response |
|---|---|
| `POST /login` | `{tenant, email, password}` → `{token, token_type: "Bearer", expires_in}` (rate limited; 5 failures lock the user 15 min) |
| `GET /me` | → `{tenant, user: {id, email, full_name, is_admin, roles}, default_company, scopes, fiscal_year_start_month, packages}` |
| `POST /logout_all` | revokes every session of the caller |
| `POST /password` | `{current_password, new_password}` (policy: ≥10 chars, 3 of lower/upper/digit/symbol) |
| `GET /api_keys`, `POST /api_keys` `{name, expires_at?}` → `{id, key}`, `DELETE /api_keys/{id}` | API keys (secret shown once) |

## Metadata — `/api/meta`

* `GET /api/meta` → `{version, packages, modules: [{name, label, icon, order, items: ["EntityName" | "report:<name>"]}], entities: [{name, label, label_plural, kind, module, icon, submittable, custom, permissions}], warnings}`
* `GET /api/meta/{Entity}` → full description used to render forms and lists:
  `{name, label, label_plural, kind (master|document|child|settings|ledger), module, title_field, submittable, company_scoped, numbering, list: {columns, sort}, form: [{label, fields, columns, collapsible}], search_fields, rules, actions: [{name, label, docstatus[], permission, confirm?, condition?}], workflow?: {initial_state, states: [{name, docstatus, edit_roles}], transitions: [{action, from, to, roles, condition?, not_creator}]}, fields: [Field], children: {tableField: {entity, label, fields: [Field]}}, permissions: {read|write|create|delete|submit|cancel|export|import: "none"|"own"|"all"}}`
* `Field`: `{name, label, type, required?, unique?, read_only?, hidden?, in_list?, in_filter?, options?, to?, of?, precision?, currency?, min?, max?, max_length?, default?, default_expr?, computed?, visible_if?, required_if?, read_only_if?, fetch_from?, description?, link_filters?, storage: "column"|"ext", can_read, can_write}`.
  Types: `data text markdown code int decimal currency percent date datetime check select link table json email phone url attach color`.
  `visible_if`/`required_if`/`read_only_if` are CEL over `doc` (and `parent` in child rows); the server is the authority.

## Documents — `/api/v1/{Entity}`

| Method & path | Notes |
|---|---|
| `GET /api/v1/{E}` | Query: `filters` (JSON: `[["field","op",value],...]` or `{"field": value}`; ops `= != > >= < <= in "not in" like "not like" between is`; paths `child_table.field`, `link_field.target_field`), `fields` (comma list), `sort` (`field asc|desc`), `limit` (≤500), `offset`, `cursor` (from `next_cursor`, for `created_at`/`updated_at` sorts), `search`, `titles`, `count` → `{rows, next_cursor?, total?}` |
| `POST /api/v1/{E}` | create (draft for submittable entities) → `201` record |
| `GET /api/v1/{E}/{id}` | `titles` defaults to true |
| `PATCH /api/v1/{E}/{id}` | partial update; child tables are replaced by the given rows (rows with a known `id` are patched, rows without `id` are added, missing rows are removed) |
| `DELETE /api/v1/{E}/{id}` | drafts/cancelled/masters → `204` |
| `POST /api/v1/{E}/{id}/submit` · `/cancel` · `/amend` | lifecycle; amend returns the new draft |
| `POST /api/v1/{E}/{id}/actions/{action}` | custom actions from metadata (e.g. `make_delivery_note`); body = params; returns action JSON (often `{entity, id}` of a created record) |
| `POST /api/v1/{E}/{id}/workflow/{action}` | workflow transition |
| `POST /api/v1/{E}/_calc` | preview: runs defaults, fetches and calculation hooks on an unsaved body (include `id` to start from a saved record) → computed document, nothing written. Use for live totals |
| `GET /api/v1/{E}/{id}/print` | HTML print format |
| `GET /api/v1/{E}/{id}/audit` | `{events: [{at, actor, actor_name, action, version, diff}], chain_valid}` |
| `GET /api/v1/{E}/_export` | CSV (export permission); same filters as list |
| `POST /api/v1/{E}/_import` | JSON array (≤5000) → `{created, ids, errors: [{row, error, fields?}]}` (`207` if some failed) |

## Reports — `/api/reports`

* `GET /api/reports` → `{reports: [{name, label, module, params: [{name, label, type: "link"|"date"|"select"|"check", to?, options?, required?, default?}]}]}`
* `GET /api/reports/{name}?param=value...` → `{name, label, columns: [{name, label, type}], rows: [{..., _indent?, _bold?}], totals?: {...}, meta?: {...}}`

## Administration (administrators only, except notifications)

`/api/admin/users` (GET, POST `{email, full_name, password?, roles[], is_admin}`), `/api/admin/users/{id}/roles` (PUT `{roles}`),
`/scopes` (PUT `{entity, ids[], default?}`), `/active` (PUT `{active}`), `/password` (PUT `{password}`);
`/api/admin/roles` (GET), `/api/admin/roles/{role}/permissions` (PUT PermissionDef `{entity, read, write, create, delete, submit, cancel, export, import, if_owner, read_groups[], write_groups[]}`);
`/api/admin/packages` (GET, POST `{packages[]}`); `/api/admin/webhooks` (GET, POST `{name, url, topics[]}` → `{id, secret}`, DELETE `/{id}`);
`/api/admin/automations` (GET, POST `{name, entity, event: created|updated|submitted|cancelled|deleted, condition?, actions: [...] , is_active}`, DELETE `/{id}`).
Automation actions: `{"type":"set_fields","values":{field: CEL}}`, `{"type":"create_document","entity":E,"values":{field: CEL}}`, `{"type":"notify","to":"owner|role:<name>|user:<id>","title":CEL,"body"?:CEL}`, `{"type":"email","to":CEL,"subject":CEL,"body":CEL}`.

Customization: `GET /api/customize` → `{customizations: [{id, seq, entity, op, is_active, note, created_at}], warnings}`;
`POST /api/customize` `{entity, op, note?}`; `PUT /api/customize/{id}/active` `{active}`; `GET /api/customize/export`; `POST /api/customize/import`.
Ops: `{"op":"add_field","field":Field,"after"?:name}` (custom field names start with `x_`), `{"op":"set_property","field","property","value"}` (label, description, required, hidden, read_only, in_list, in_filter, default, default_expr, visible_if, required_if, read_only_if, max_length, min, max), `{"op":"add_rule","rule":{name, check: CEL, message, on: save|submit}}`, `{"op":"add_options","field","options":[...]}`, `{"op":"set_entity_property","property":"label|label_plural|description|icon|title_field|list|search_fields|form","value"}`, `{"op":"define_entity","entity":EntityDef}` (name starts with `X`), `{"op":"set_workflow","workflow":WorkflowDef}`, `{"op":"set_print_template","template":"<jinja html>"}`.

Plugins (sandboxed WebAssembly, see README): `GET /api/admin/plugins` → `{plugins: [{id, name, version, description, sha256, hooks, fuel_limit, memory_limit_mb, enabled, updated_at}]}`;
`POST /api/admin/plugins` `{name, version?, description?, hooks: [{entity|"*", stage: before_validate|validate|before_submit|on_submit|before_cancel|on_cancel|before_delete}], fuel_limit?, memory_limit_mb?, wasm_base64}` → `{id}` (same name replaces);
`PUT /api/admin/plugins/{id}/enabled` `{enabled}`; `DELETE /api/admin/plugins/{id}`.

Notifications (own): `GET /api/notifications`, `POST /api/notifications/{id}/read`.

## AI agents (MCP)

`POST /mcp` — Model Context Protocol, Streamable HTTP, stateless (JSON responses, no session id;
`GET`/`DELETE` → 405). Auth as above (`Bearer` session token or API key); 401 carries
`WWW-Authenticate`. Methods: `initialize` (versions 2025-06-18, 2025-03-26, 2024-11-05), `ping`,
`tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`
(`ferrum://overview`, `ferrum://entity/{name}`, `ferrum://record/{entity}/{id}`), `prompts/list`,
`prompts/get`. Tool failures come back as results with `isError: true` and the same error JSON as
the REST API; unknown tools or bad arguments are JSON-RPC errors (-32602).

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

`POST /api/files` (multipart: `file`, optional `entity`, `record_id`; ≤25 MB) → `{id, url: "/files/<id>", file_name, size, sha256}`; `GET /files/{id}` (authenticated download).

## System

`GET /api/ready` (DB reachable), `GET /api/version`, `GET /metrics` (Prometheus), `POST /api/tenants` (requires `X-Ferrum-Bootstrap-Token`), `GET /_ping`, `GET /_health`.

## ERPNext compatibility

`GET|POST /api/resource/{DocType}`, `GET|PUT|DELETE /api/resource/{DocType}/{name}` (`name` = document number or id; DocType names map by removing spaces), `GET /api/method/frappe.auth.get_logged_user`. Responses use `{"data": ...}`.
