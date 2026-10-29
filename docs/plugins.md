# Plugins: turning Ferrum into any platform

A plugin is a WebAssembly module plus a manifest. It can validate and enrich documents, bring its
own entities, serve HTTP routes (signed-in or public), hold WebSocket connections to devices,
react to events, run on a schedule and call declared HTTPS services — all inside a sandbox, with
only the permissions an administrator approved. That is enough to turn Ferrum into an EV charging
system (OCPP over WebSocket, OCPI over HTTPS), an IoT platform, a payment or telephony gateway,
without changing Ferrum itself.

`plugins/examples/device-gateway` is a complete example (devices over WebSocket, commands from
users, readings as records, a public status route, an event subscription, a schedule and a
configurable hook); `plugins/sdk` is the Rust SDK it uses.

## Writing one (Rust)

```rust
use ferrum_plugin_sdk::{plugin, Request, json, Value, data, kv, send};

plugin!(handle);

fn handle(req: Request) -> Value {
    match req.kind() {
        "socket" => json!({"send": "{\"type\":\"accepted\"}"}),
        "http" => json!({"json": {"hello": req.user_email()}}),
        _ => json!({}),
    }
}
```

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

Unit tests run natively (`cargo test`): `ferrum_plugin_sdk::testing::host(|op, args| …)` answers
the plugin's host calls (`Ok(value)` or `Err((code, message))`), so a test can feed an input to
the handler and check every record, key-value and HTTP call it made. The Commerce app's payment
gateways (`gateways/` in its repository: Razorpay and Stripe) are complete examples.

Any language that compiles to a core WebAssembly module works: the contract is below.

## Installing

`POST /api/admin/plugins` (administrators) with the manifest fields plus `wasm_base64`. Installing
again with the same `name` upgrades it in place. `PUT /api/admin/plugins/{id}/enabled`
`{"enabled": false}` switches it off at once; `PUT /api/admin/plugins/{id}/config` `{"config": {...}}`
replaces its settings and secrets (listings only show the keys); `DELETE /api/admin/plugins/{id}`
removes it with its private data (records it created stay).

## Manifest

| Field | Meaning |
|---|---|
| `name`, `version`, `description` | `name`: 2–40 lower-case letters, digits or `-` |
| `config` | Settings and secrets, delivered in every input as `config` |
| `entities` | Entity definitions it brings (masters or documents, names start with `X`), stored without DDL |
| `permissions` | `[{entity, read, write, create, delete, submit, cancel}]` — its service user's role, and the most it may do on a user's behalf |
| `hooks` | `[{entity, stage}]` with stage `before_validate`, `validate`, `before_submit`, `on_submit`, `before_cancel`, `on_cancel`, `before_delete`; entity `*` for all |
| `routes` | `[{path, public}]`; `/x` or `/x/*`. Signed-in: `/api/ext/{plugin}{path}`; public: `/api/public/{tenant}/{plugin}{path}` |
| `sockets` | `[{path, public, subprotocols}]` at `/ws/ext/{tenant}/{plugin}{path}`; clients must offer one of the subprotocols |
| `subscriptions` | Outbox topics: `SalesInvoice.submitted`, `SalesInvoice.*`, `plugin.other.*`, `*` |
| `schedules` | `[{name, every_seconds}]` (at least 60) |
| `http_hosts` | Hosts it may call: `api.example.com`, `*.example.com` |
| `fuel_limit`, `memory_limit_mb` | Per-call CPU budget (default 50 M) and memory cap (default 32 MB) |

## Contract

The module may import only `ferrum.host_call(ptr, len) -> i64` and must export `memory`,
`ferrum_alloc(len) -> ptr` and `ferrum_hook(ptr, len) -> (ptr << 32 | len)`. Every call is a
fresh instance; input and output are UTF-8 JSON.

| `kind` | Input (besides `kind`, `config`) | Reply |
|---|---|---|
| `hook` | `entity`, `stage`, `doc`, `user` | `{}`, `{"patch": {...}}` (before_validate), `{"error"}` or `{"errors": [{field, message, row?}]}` |
| `http` | `method`, `path`, `query`, `headers` (no credentials), `body`, `user` (null when public) | `{"status", "headers", "body"}` or `{"status", "json"}` |
| `event` | `topic`, `event` (the outbox payload) | `{}`; an error makes the event retry |
| `schedule` | `name` | `{}` |
| `socket` | `event` (`connect`/`message`/`close`), `connection`, `path`, `query`, `subprotocol`, `message`, `user` | `{"send": text or [texts], "close": bool}` |

**Identity.** Hooks run inside the document's transaction as the user; signed-in routes and
sockets run as the user. In both cases the plugin is also limited to its declared permissions.
Public routes, public sockets, events and schedules run as the plugin's own service user, whose
role is exactly its declared permissions. Everything it changes lands in the audit trail.

## Host calls

`host_call` takes `{"op", "args"}` and returns `{"ok": value}` or
`{"error": {"code", "message", "fields"?}}`.

| `op` | `args` → result |
|---|---|
| `data.get` | `{entity, id}` → record |
| `data.query` | `{entity, filters, fields?, sort?, limit?, cursor?}` → `{rows, next_cursor?}` |
| `data.insert` / `data.update` / `data.delete` | `{entity, data}` / `{entity, id, data, expected_version?}` / `{entity, id}` |
| `data.submit` / `data.cancel` / `data.action` | `{entity, id}` / `{entity, id}` / `{entity, id, action, params}` |
| `kv.get` / `kv.set` / `kv.delete` / `kv.list` | private per-tenant storage: `{key}` / `{key, value}` / `{key}` / `{prefix}` |
| `http.fetch` | `{method, url, headers?, body?}` → `{status, headers, body}`; declared hosts only, https only, never private addresses, no redirects, 15 s, 1 MB |
| `events.emit` | `{topic, payload}` → publishes `plugin.{name}.{topic}` (inside the document's transaction for hooks) |
| `sockets.send` / `sockets.close` | `{connection, message}` / `{connection, reason}` — on whichever replica holds the socket |
| `log` | `{level?, message}`: `info` (default), `warn` or `error`, to the server log with the plugin and tenant |
| `time.now` | → RFC 3339 UTC |

## Limits and guarantees

* No ambient access: no WASI, no files, no network except declared hosts, no clock except `time.now`.
* Fuel (deterministic CPU), memory cap, 1 000 host calls and 1 MB of output per call; nested
  plugin work stops after 4 levels.
* Failures are fail-closed and logged; disabling a plugin takes effect within seconds on every replica.
* Stateless: plugin state lives in PostgreSQL (`kv`, its entities). Events are delivered at least
  once and recorded per plugin, schedules fire on one replica at a time, socket messages reach the
  replica holding the connection (cross-replica messages up to 7 KB). WebSockets are rate limited
  (100 messages per 10 s), text only, 64 KB per message, with keepalive pings.

## EV charging, for example

An OCPP 1.6J plugin declares `sockets: [{path: "/ocpp/*", public: true, subprotocols: ["ocpp1.6"]}]`
and entities such as `XChargePoint`, `XConnector` and `XChargingSession`. `BootNotification`,
`Heartbeat`, `StatusNotification`, `StartTransaction`, `MeterValues` and `StopTransaction` arrive
as socket messages and become records; operators' "start charging" and "unlock" buttons are
routes that `sockets.send` a `RemoteStartTransaction` or `UnlockConnector` to the charge point.
A schedule marks silent charge points offline, an event subscription turns finished sessions into
sales invoices, and OCPI roaming is HTTPS to declared partner hosts plus public routes for their
callbacks.
