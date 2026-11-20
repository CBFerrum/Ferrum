# Agent parity

Every change an app makes to data on a person's request goes through a declared operation: a record operation or an
action with a contract, run through the kernel's record pipeline or the gate, so that an agent can do what a screen does
by the same operation (docs/app-contract.md, docs/guides/06). `scripts/agent_parity.py` keeps this file true: CI fails
on a write route or an action the tables lack, a row the code no longer has, a row without a kind, or more debt than the
numbers below allow. `--write` rewrites the tables from the code, keeps each kind given, writes `unclassified` for a new
route (which fails until a person gives it a kind), and lowers the two numbers to today's counts; it never raises them.

A route is listed when it writes: every write route of the platform's controllers, and every arm of an app's `Site` that
reaches a write (the kernel's record calls, the outbox, SQL that inserts, updates or deletes), whether or not it also
calls the gate: a route that runs an operation and then writes something of its own does what an agent would not. Hooks,
event sinks, upkeep and jobs are not routes: an agent triggers them by the same operation a person does.

| Kind | Meaning | Debt |
|---|---|---|
| `operation: <capability or tool>` | the same kernel call as a catalog operation (`*.create`, `*.update`, `*.delete`, `*.submit`, `*.cancel`, `*.<action>`, `*.transition.<action>`) or a named MCP tool | no |
| `identity` | sign-in, sessions, passkeys, OAuth, SAML, SCIM | no |
| `protocol: <name>` | a protocol an outside program speaks: DAV, WOPI, an inbound provider webhook, `/mcp` itself | no |
| `public` | anonymous visitors on `/api/site/…` or signed links | no |
| `portal` | outside people signed in to the portal acting on their own records | no |
| `administration` | platform configuration (settings, users, roles, vault, apps); agents reach it under their grants | no |
| `collaboration` | comments, assignments, tags, follows on any record | no |
| `debt` | must become an operation; the allowed number only falls | yes |

The tables shrink when a route's writes move into an action with a contract that the route invokes, and when an action
gets a contract with `effect`, `returns` and `completion` (the recipe in docs/guides/06). The next `--write` lowers the
numbers.

The kinds were given by hand on 18 November 2026: each platform route by its controller and handler (a person's own
notifications, preferences, saved views and away state counted as collaboration; the record routes an agent has no tool
for yet, imports, bulk changes, merges, approvals, mailing a record, printing labels, files and jobs, as debt); each app
route by who calls it (anonymous visitors, portal people, an outside program) and as debt otherwise, but for
Intelligence's feedback and accept routes, which make the record calls themselves.

Routes in debt allowed: 148
Actions without a contract allowed: 595

## Routes

| Where | Method | Route | Handler | Kind |
|---|---|---|---|---|
| `assets:src/ops.rs` | POST | `["lockouts", id, "return"]` | `return_to_service` | debt |
| `assets:src/ops.rs` | POST | `["lockouts"]` | `lock` | debt |
| `assets:src/ops.rs` | POST | `["requests", id, "repaired"]` | `repaired` | debt |
| `bookings:src/site.rs` | POST | `["book"]` | `throttle` | public |
| `bookings:src/site.rs` | POST | `["manage", tok, "cancel"]` | `throttle` | public |
| `bookings:src/site.rs` | POST | `["manage", tok, "reschedule"]` | `throttle` | public |
| `campaigns:src/public.rs` | POST | `["confirm", token]` | `confirm` | public |
| `campaigns:src/public.rs` | POST | `["reconfirm", token]` | `reconfirm` | public |
| `campaigns:src/public.rs` | POST | `["subscribe", form]` | `subscribe` | public |
| `campaigns:src/public.rs` | POST | `["u", token]` | `unsubscribe` | public |
| `campaigns:src/public.rs` | POST | `["webhooks", "bounces"]` | `webhook` | protocol: a mail provider's bounce webhook |
| `commerce:src/site.rs` | DELETE | `["cart", "codes", code]` | `cart_op` | public |
| `commerce:src/site.rs` | DELETE | `["cart", "coupon"]` | `cart_op` | public |
| `commerce:src/site.rs` | DELETE | `["cart", "items", line]` | `cart_op` | public |
| `commerce:src/site.rs` | POST | `["cart", "items"]` | `cart_op` | public |
| `commerce:src/site.rs` | POST | `["cart"]` | `create_cart` | public |
| `commerce:src/site.rs` | POST | `["checkout"]` | `checkout::checkout` | public |
| `commerce:src/site.rs` | POST | `["desk", "change-preview"]` | `edit::preview` | debt |
| `commerce:src/site.rs` | POST | `["desk", "refund-plan"]` | `refund::plan` | debt |
| `commerce:src/site.rs` | POST | `["items", slug, "reviews"]` | `catalog::submit_review` | public |
| `commerce:src/site.rs` | POST | `["items", slug, "stock-alert"]` | `load` | public |
| `commerce:src/site.rs` | POST | `["orders", "pay"]` | `pay` | public |
| `commerce:src/site.rs` | POST | `["orders", number, "pay"]` | `pay` | portal |
| `commerce:src/site.rs` | POST | `["orders", number, "reorder"]` | `load` | portal |
| `commerce:src/site.rs` | POST | `["subscriptions", number, "cancel"]` | `subscriptions::cancel` | portal |
| `commerce:src/site.rs` | POST | `["subscriptions", number, "pay"]` | `load` | portal |
| `commerce:src/site.rs` | PUT | `["cart", "coupon"]` | `cart_op` | public |
| `commerce:src/site.rs` | PUT | `["cart", "shipping"]` | `cart_op` | public |
| `commerce:src/site.rs` | PUT/PATCH | `["cart", "details"]` | `cart_op` | public |
| `commerce:src/site.rs` | PUT/PATCH | `["cart", "items", line]` | `cart_op` | public |
| `comms:src/calendar.rs` | DELETE | `["events", e]` | `shared_delete` | debt |
| `comms:src/calendar.rs` | DELETE | `f` | `delete_tx` | debt |
| `comms:src/calendar.rs` | PATCH | `["events", e]` | `shared_edit` | debt |
| `comms:src/calendar.rs` | PATCH/DELETE | `["calendars", c]` | `crate::calendars::change` | debt |
| `comms:src/calendar.rs` | POST | `["calendars"]` | `crate::calendars::change` | debt |
| `comms:src/calendar.rs` | POST | `["events", e, "proposals", p]` | `crate::proposals::decide` | debt |
| `comms:src/calendar.rs` | POST | `["events", e, "propose"]` | `crate::proposals::propose` | debt |
| `comms:src/calendar.rs` | POST | `["events", e, "rsvp"]` | `rsvp` | debt |
| `comms:src/calendar.rs` | POST | `["events"]` | `create_event` | debt |
| `comms:src/calendar.rs` | POST | `["import"]` | `import` | debt |
| `comms:src/calendar.rs` | POST | `["subscriptions", c, "refresh"]` | `crate::calendars::refresh` | debt |
| `comms:src/calendar.rs` | POST | `["subscriptions"]` | `crate::subscribe::subscribe` | debt |
| `comms:src/calendar.rs` | POST | `_` | `util::doc_id` | debt |
| `comms:src/calendar.rs` | PUT | `["hours"]` | `crate::hours::handle` | debt |
| `comms:src/calendars.rs` | DELETE | `Some(id)` | `mine` | debt |
| `comms:src/calendars.rs` | PATCH | `Some(id)` | `mine` | debt |
| `comms:src/calendars.rs` | POST | `None` | `fields` | debt |
| `comms:src/chat.rs` | DELETE | `["messages", m]` | `delete` | debt |
| `comms:src/chat.rs` | PATCH | `["channels", c]` | `update_channel` | debt |
| `comms:src/chat.rs` | PATCH | `["messages", m]` | `edit` | debt |
| `comms:src/chat.rs` | POST | `["channels", c, "join"]` | `join` | debt |
| `comms:src/chat.rs` | POST | `["channels", c, "leave"]` | `leave` | debt |
| `comms:src/chat.rs` | POST | `["channels", c, "members"]` | `change_members` | debt |
| `comms:src/chat.rs` | POST | `["channels", c, "messages"]` | `post` | debt |
| `comms:src/chat.rs` | POST | `["channels", c, "read"]` | `mark_read` | debt |
| `comms:src/chat.rs` | POST | `["channels"]` | `create_channel` | debt |
| `comms:src/chat.rs` | POST | `["direct"]` | `direct` | debt |
| `comms:src/chat.rs` | POST | `["read-all"]` | `mark_all_read` | debt |
| `comms:src/chat.rs` | PUT | `["channels", c, "notify"]` | `set_notify` | debt |
| `comms:src/dav.rs` | DELETE | `"DELETE" => delete(k, ctx, req, &res).await,` | `?` | protocol: DAV |
| `comms:src/dav.rs` | PUT | `"PUT" => put(k, ctx, req, &res).await,` | `?` | protocol: DAV |
| `comms:src/hours.rs` | PUT | `Some(body)` | `from_body` | debt |
| `comms:src/meet.rs` | DELETE | `["room", id, "guest-link"]` | `guest_link` | debt |
| `comms:src/meet.rs` | POST | `["room", id, "guest-link"]` | `guest_link` | debt |
| `comms:src/meet.rs` | POST | `["rooms"]` | `new_room` | debt |
| `comms:src/meet.rs` | POST | `[kind, id, "recordings", rec, "stop"]` | `stop_recording` | debt |
| `comms:src/meet.rs` | POST | `[kind, id, "recordings"]` | `start_recording` | debt |
| `comms:src/meet.rs` | POST | `[kind, id, "token"]` | `server` | debt |
| `comms:src/meet.rs` | POST | `[kind, id, "transcript"]` | `said` | debt |
| `comms:src/rooms.rs` | DELETE | `[id]` | `parse` | debt |
| `comms:src/rooms.rs` | PATCH | `[id]` | `parse` | debt |
| `comms:src/rooms.rs` | POST | `[]` | `util::body_json` | debt |
| `comms:src/rooms.rs` | POST/PATCH/DELETE | `(m, rest) if ["POST", "PATCH", "DELETE"].contains(&m) => {` | `?` | debt |
| `comms:src/site.rs` | DELETE | `["domain", "senders", who]` | `mailadmin::remove_sender` | debt |
| `comms:src/site.rs` | POST | `["communications", c, "retry"]` | `retry` | debt |
| `comms:src/site.rs` | POST | `["communications", c, "seen"]` | `mail::mark_seen` | debt |
| `comms:src/site.rs` | POST | `["domain", "senders"]` | `mailadmin::add_sender` | debt |
| `comms:src/site.rs` | POST | `["inbound", token]` | `inbound::webhook` | protocol: a mail provider's inbound webhook |
| `comms:src/site.rs` | POST | `["meet", "guest"]` | `meet::guest` | public |
| `comms:src/site.rs` | POST | `["rsvp"]` | `calendar::answer` | public |
| `comms:src/site.rs` | POST | `["send"]` | `mail::send` | debt |
| `comms:src/webmail.rs` | POST | `["file"]` | `file` | debt |
| `comms:src/webmail.rs` | POST | `["itip"]` | `itip::from_message` | debt |
| `comms:src/webmail.rs` | POST | `["jmap"]` | `jmap` | protocol: JMAP |
| `comms:src/webmail.rs` | POST | `["unsubscribe"]` | `unsubscribe` | debt |
| `comms:src/webmail.rs` | POST | `["upload", account]` | `send` | debt |
| `comms:src/webmail.rs` | PUT | `["filing", "private"]` | `arrivals::set_private` | debt |
| `comms:src/webmail.rs` | PUT | `["filing"]` | `arrivals::set_filing` | debt |
| `control:src/site.rs` | POST | `"case", Some(u)` | `case` | portal |
| `control:src/site.rs` | POST | `"partner/onboard", Some(u)` | `onboard` | portal |
| `control:src/site.rs` | POST | `"report", _` | `report` | public |
| `drive:src/office.rs` | POST | `false, op` | `lock` | protocol: WOPI |
| `drive:src/office.rs` | POST | `true, "PUT"` | `put_file` | protocol: WOPI |
| `drive:src/sharing.rs` | POST | `["links", token, "unlock"]` | `unlock` | public |
| `drive:src/site.rs` | POST | `["adopt"]` | `tree::ensure_space` | debt |
| `drive:src/site.rs` | POST | `["editor", file, "lock"]` | `crate::office::browser_lock` | debt |
| `drive:src/site.rs` | POST | `["editor", file]` | `crate::office::launch` | debt |
| `drive:src/site.rs` | POST | `["editor", file]` | `crate::office::launch_browser` | debt |
| `drive:src/site.rs` | POST | `["files", file, "images"]` | `crate::images::upload` | debt |
| `drive:src/site.rs` | POST | `["files", file, "versions"]` | `files::store` | debt |
| `drive:src/site.rs` | POST | `["presence", file]` | `crate::collab::heartbeat` | collaboration |
| `drive:src/site.rs` | POST | `[verb @ ("upload" \| "new")]` | `tree::ensure_space` | debt |
| `edge:src/portal.rs` | POST | `["alarms", id, "acknowledge"]` | `acknowledge` | portal |
| `edge:src/site.rs` | DELETE | `["boards", id, "share"]` | `set_share` | debt |
| `edge:src/site.rs` | DELETE | `["definitions", id, "provisioning"]` | `allow_joining` | debt |
| `edge:src/site.rs` | DELETE | `["devices", id, "credentials"]` | `revoke` | debt |
| `edge:src/site.rs` | POST | `"config"` | `gateway_config` | protocol: the Edge gateway's |
| `edge:src/site.rs` | POST | `"ingest"` | `ingest` | protocol: the Edge gateway's |
| `edge:src/site.rs` | POST | `["alarms", id, "acknowledge"]` | `crate::alarms::acknowledge` | debt |
| `edge:src/site.rs` | POST | `["alarms", id, "assign"]` | `crate::alarms::assign` | debt |
| `edge:src/site.rs` | POST | `["alarms", id, "clear"]` | `crate::alarms::clear` | debt |
| `edge:src/site.rs` | POST | `["alarms", id, "note"]` | `crate::alarms::comment` | debt |
| `edge:src/site.rs` | POST | `["alarms", id, "shelve"]` | `crate::alarms::shelve` | debt |
| `edge:src/site.rs` | POST | `["boards", "data"]` | `crate::data::board` | debt |
| `edge:src/site.rs` | POST | `["boards", id, "share"]` | `share` | debt |
| `edge:src/site.rs` | POST | `["charging", id, "settle"]` | `crate::charging::settle` | debt |
| `edge:src/site.rs` | POST | `["definitions", "try"]` | `may` | debt |
| `edge:src/site.rs` | POST | `["definitions", id, "provisioning"]` | `allow_joining` | debt |
| `edge:src/site.rs` | POST | `["devices", "claim"]` | `fleet::claim` | debt |
| `edge:src/site.rs` | POST | `["devices", "import"]` | `fleet::import` | debt |
| `edge:src/site.rs` | POST | `["devices", id, "credentials"]` | `credentials` | debt |
| `edge:src/site.rs` | POST | `["devices", id, "token"]` | `new_token` | debt |
| `edge:src/site.rs` | POST | `["fleet", "command"]` | `fleet::command` | debt |
| `edge:src/site.rs` | POST | `["fleet", "desired"]` | `fleet::desire` | debt |
| `edge:src/site.rs` | POST | `["flows", "try"]` | `may` | debt |
| `edge:src/site.rs` | POST | `["flows", id, "simulate"]` | `simulate_flow` | debt |
| `edge:src/site.rs` | POST | `["solutions", "install"]` | `crate::bundles::install` | debt |
| `edge:src/site.rs` | POST | `["strangers", "claim"]` | `claim` | debt |
| `edge:src/site.rs` | PUT | `["devices", id, "desired"]` | `desire` | debt |
| `education:src/site.rs` | POST | `["apply"]` | `apply` | public |
| `erp:addons/gb/src/mtd.rs` | POST | `["returns", rid, "file"]` | `parse` | debt |
| `erp:addons/mes/src/lib.rs` | POST | `["revisions", revision, "approve"]` | `runs::approve` | debt |
| `erp:addons/mes/src/lib.rs` | POST | `["runs", run, "events"]` | `runs::record` | debt |
| `erp:addons/mes/src/lib.rs` | POST | `["runs", run, "materials"]` | `runs::confirm_materials` | debt |
| `erp:addons/mes/src/lib.rs` | POST | `["runs", run, "release"]` | `runs::release` | debt |
| `erp:addons/quality/src/lib.rs` | POST | `["capa", id, "verify"]` | `verify_capa` | debt |
| `erp:addons/quality/src/lib.rs` | POST | `["holds", id, "release"]` | `release` | debt |
| `erp:addons/quality/src/lib.rs` | POST | `["ncr", id, "disposition"]` | `decide` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["billing"]` | `billing::bill` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["counts", id, "approve"]` | `counts::approve` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["counts", id, "enter"]` | `counts::enter` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["counts", id, "finish"]` | `counts::finish` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["counts", id, "start"]` | `counts::start` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["inbound", id, "close"]` | `inbound::close` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["inbound", id, "scan"]` | `inbound::scan` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["inbound", id, "settle"]` | `inbound::settle` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["inbound", id, "unknown"]` | `inbound::unknown` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["positions", "move"]` | `may` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["positions", "status"]` | `may` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["returns", id, "dispose"]` | `returns::dispose` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["returns", id, "receive"]` | `returns::receive` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["shipments", id, "hand-over"]` | `shipping::hand_over` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["shipments", id, "verify"]` | `shipping::verify` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["tasks", id, "cancel"]` | `tasks::cancel` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["tasks", id, "confirm"]` | `tasks::confirm` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["tasks", id, "recover"]` | `tasks::recover` | debt |
| `erp:addons/wms/src/site.rs` | POST | `["tasks", id, "release"]` | `tasks::release` | debt |
| `field-service:src/site.rs` | POST | `None, ["requests"]` | `public_request` | public |
| `forms:src/site.rs` | POST | `["f", slug, "responses"]` | `submit` | public |
| `government:src/site.rs` | POST | `["portal", "applications", a, "appeal"]` | `act` | portal |
| `government:src/site.rs` | POST | `["portal", "applications", a, "documents"]` | `attach` | portal |
| `government:src/site.rs` | POST | `["portal", "applications", a, "submit"]` | `act` | portal |
| `government:src/site.rs` | POST | `["portal", "applications", a, "withdraw"]` | `act` | portal |
| `government:src/site.rs` | POST | `["portal", "applications"]` | `draft` | portal |
| `government:src/site.rs` | POST | `["portal", "cases"]` | `raise` | portal |
| `government:src/site.rs` | POST | `["portal", "permits", p, "renew"]` | `renew` | portal |
| `healthcare:src/site.rs` | POST | `["book"]` | `throttle` | public |
| `healthcare:src/site.rs` | POST | `["manage", tok, "cancel"]` | `throttle` | public |
| `healthcare:src/site.rs` | POST | `["portal", "cancel"]` | `cancel_mine` | portal |
| `healthcare:src/site.rs` | POST | `["portal", "consent"]` | `consent` | portal |
| `helpdesk:src/site.rs` | POST | `["portal", "articles", a, "vote"]` | `vote` | portal |
| `helpdesk:src/site.rs` | POST | `["portal", "tickets", t, "rating"]` | `rate` | portal |
| `helpdesk:src/site.rs` | POST | `["portal", "tickets", t, "replies"]` | `reply` | portal |
| `helpdesk:src/site.rs` | POST | `["portal", "tickets"]` | `raise` | portal |
| `hospitality:src/site.rs` | POST | `["book"]` | `throttle` | public |
| `hospitality:src/site.rs` | POST | `["manage", tok, "cancel"]` | `throttle` | public |
| `hr:src/site.rs` | POST | `["careers", o, "apply"]` | `apply` | public |
| `intelligence:src/site.rs` | POST | `"ask"` | `ask` | debt |
| `intelligence:src/site.rs` | POST | `"feedback"` | `body` | operation: *.create |
| `intelligence:src/site.rs` | POST | `path` | `trim_start_matches` | operation: *.accept |
| `live:src/site.rs` | POST | `["broadcasts", id, rest @ ..]` | `session::watch` | debt |
| `lms:src/lti.rs` | POST | `["scores"]` | `record_score` | protocol: LTI |
| `lms:src/site.rs` | POST | `["lti", "token"]` | `lti::token` | protocol: LTI |
| `lms:src/xapi.rs` | DELETE | `sid` | `sqlx::query` | protocol: xAPI |
| `lms:src/xapi.rs` | PUT/POST | `"PUT" \| "POST" => {` | `?` | protocol: xAPI |
| `lms:src/xapi.rs` | PUT/POST | `Some(sid)` | `header` | protocol: xAPI |
| `metrics:src/site.rs` | POST | `"import/wren"` | `crate::wren::import` | debt |
| `ops:src/site.rs` | POST | `"enroll"` | `identity::enroll` | protocol: a device's enrolment and sync |
| `ops:src/site.rs` | POST | `"sync"` | `identity::verify` | protocol: a device's enrolment and sync |
| `ops:src/site.rs` | POST | `["acceptance", run, "steps", step]` | `device_site` | debt |
| `ops:src/site.rs` | POST | `["authority", "pool"]` | `authority::pool` | debt |
| `ops:src/site.rs` | POST | `["authority", "transfer"]` | `device_site` | debt |
| `ops:src/site.rs` | POST | `["commands", id, act @ ("approve" \| "refuse" \| "cancel" \| "reconcile")]` | `command_site` | debt |
| `ops:src/site.rs` | POST | `["commands"]` | `body` | debt |
| `ops:src/site.rs` | POST | `["devices", id, "acceptance"]` | `device_site` | debt |
| `ops:src/site.rs` | POST | `["devices", id, "enrollment"]` | `device_site` | debt |
| `ops:src/site.rs` | POST | `["devices", id, "lifecycle"]` | `device_site` | debt |
| `ops:src/site.rs` | POST | `["operators", "pin"]` | `site_open` | debt |
| `pos:src/site.rs` | POST | `["approve"]` | `approve` | debt |
| `pos:src/site.rs` | POST | `["exchange"]` | `crate::invoice::exchange` | debt |
| `pos:src/site.rs` | POST | `["pin"]` | `crate::limits::set_pin` | debt |
| `pos:src/site.rs` | POST | `["reprint"]` | `crate::receipt::reprint` | debt |
| `pos:src/site.rs` | POST | `["send"]` | `crate::receipt::send` | debt |
| `property:src/site.rs` | POST | `["portal", "requests"]` | `raise` | portal |
| `retail:src/site.rs` | POST | `["portal", "reports"]` | `report` | portal |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/code` | `send_code` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/continue` | `proceed` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/join` | `join` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/join/code` | `join_code` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/password` | `set_password` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/region-token` | `region_token` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/sign-in` | `sign_in` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/sign-in/second-factor` | `second_factor` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/sign-in/verify` | `verify` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/sign-out-everywhere` | `sign_out_everywhere` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/signup` | `sign_up` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/signup/code` | `signup_code` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/sso/finish` | `sso_finish` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/step-up` | `step_up_answer` | identity |
| `server/src/controllers/accounts.rs` | POST | `/api/accounts/step-up/options` | `step_up_options` | identity |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/automations/{id}` | `delete_automation` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/calendars/{name}` | `delete_calendar` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/plugins/{id}` | `remove_plugin` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/retention/{id}` | `delete_retention` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/role-conflicts` | `delete_role_conflict` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/sso/{id}` | `delete_sso` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/support/{id}` | `revoke_support` | administration |
| `server/src/controllers/admin.rs` | DELETE | `/api/admin/webhooks/{id}` | `delete_webhook` | administration |
| `server/src/controllers/admin.rs` | PATCH | `/api/admin/plugins/{id}/config` | `configure_plugin` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/automations` | `save_automation` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/automations/preview` | `preview_automation` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/backfills/{package}/{name}/rerun` | `rerun_backfill` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/close` | `close_workspace` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/events/{id}/replay` | `replay_event` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/legal-holds` | `place_legal_hold` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/legal-holds/{id}/release` | `release_legal_hold` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/pack/install` | `post_pack_install` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/pack/plan` | `post_pack_plan` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/packages` | `install` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/personal-data/erase` | `erase_personal_data` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/plugins` | `install_plugin` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/restrictions` | `restrict` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/restrictions/lift` | `lift_restriction` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/retention` | `add_retention` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/retention/{id}/preview` | `preview_retention` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/sso` | `create_sso` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/support` | `grant_support` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/users` | `create_user` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/users/{id}/deprovision` | `deprovision_user` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/users/{id}/hand-over` | `hand_over_work` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/users/{id}/sso/unlink` | `unlink_sso` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/users/{id}/two-factor/reset` | `reset_two_factor` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/webhook-deliveries/{id}/replay` | `replay_webhook_delivery` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/admin/webhooks` | `create_webhook` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/customize` | `add_custom` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/customize/batch` | `batch_custom` | administration |
| `server/src/controllers/admin.rs` | POST | `/api/customize/{id}/keep` | `keep_custom` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/calendars/{name}` | `save_calendar` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/plugins/{id}/config` | `configure_plugin` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/plugins/{id}/enabled` | `toggle_plugin` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/retention/{id}` | `save_retention` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/role-conflicts` | `set_role_conflict` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/roles/{role}/permissions` | `set_permission` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/roles/{role}/portal` | `set_role_portal` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/roles/{role}/signup` | `set_role_signup` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/scim/groups/{id}/roles` | `map_scim_group` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/settings` | `put_settings` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/sso/{id}` | `update_sso` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/users/{id}/active` | `set_active` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/users/{id}/kind` | `set_kind` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/users/{id}/password` | `reset_password` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/users/{id}/roles` | `set_roles` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/admin/users/{id}/scopes` | `set_scopes` | administration |
| `server/src/controllers/admin.rs` | PUT | `/api/customize/{id}/active` | `toggle_custom` | administration |
| `server/src/controllers/apps.rs` | DELETE | `/api/admin/apps/store` | `uninstall` | administration |
| `server/src/controllers/apps.rs` | DELETE | `/api/admin/apps/upload` | `uninstall` | administration |
| `server/src/controllers/apps.rs` | DELETE | `/api/admin/apps/{id}` | `uninstall` | administration |
| `server/src/controllers/apps.rs` | DELETE | `/api/admin/apps/{id}/dev-ui` | `clear_dev_ui` | administration |
| `server/src/controllers/apps.rs` | DELETE | `/api/admin/apps/{id}/licence` | `forget_licence` | administration |
| `server/src/controllers/apps.rs` | POST | `/api/admin/apps/upload` | `upload` | administration |
| `server/src/controllers/apps.rs` | POST | `/api/admin/apps/{id}/disable` | `disable` | administration |
| `server/src/controllers/apps.rs` | POST | `/api/admin/apps/{id}/enable` | `enable` | administration |
| `server/src/controllers/apps.rs` | POST | `/api/admin/apps/{id}/install` | `install` | administration |
| `server/src/controllers/apps.rs` | POST | `/api/admin/apps/{id}/upgrade` | `upgrade` | administration |
| `server/src/controllers/apps.rs` | PUT | `/api/admin/apps/{id}/dev-ui` | `set_dev_ui` | administration |
| `server/src/controllers/apps.rs` | PUT | `/api/admin/apps/{id}/licence` | `set_licence` | administration |
| `server/src/controllers/assistant.rs` | DELETE | `/api/assistant/threads/{id}` | `remove` | protocol: the assistant's conversation |
| `server/src/controllers/assistant.rs` | POST | `/api/assistant/threads` | `create` | protocol: the assistant's conversation |
| `server/src/controllers/assistant.rs` | POST | `/api/assistant/threads/{id}/decide` | `decide` | protocol: the assistant's conversation |
| `server/src/controllers/assistant.rs` | POST | `/api/assistant/threads/{id}/messages` | `send` | protocol: the assistant's conversation |
| `server/src/controllers/assistant.rs` | POST | `/api/assistant/threads/{id}/stop` | `stop` | protocol: the assistant's conversation |
| `server/src/controllers/auth.rs` | DELETE | `/api/auth/api_keys/{id}` | `revoke_key` | identity |
| `server/src/controllers/auth.rs` | DELETE | `/api/auth/passkeys/{id}` | `remove_passkey` | identity |
| `server/src/controllers/auth.rs` | DELETE | `/api/auth/sessions/{id}` | `revoke_session` | identity |
| `server/src/controllers/auth.rs` | PATCH | `/api/auth/api_keys/{id}` | `update_key` | identity |
| `server/src/controllers/auth.rs` | PATCH | `/api/auth/passkeys/{id}` | `rename_passkey` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/2fa/begin` | `two_factor_begin` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/2fa/disable` | `two_factor_disable` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/2fa/enable` | `two_factor_enable` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/2fa/recovery-codes` | `two_factor_recovery_codes` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/api_keys` | `create_key` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/code` | `send_code` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/code/sign-in` | `sign_in_with_code` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/code/sign-up` | `sign_up` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/login` | `login` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/login/second-factor` | `login_second_factor` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/logout` | `logout` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/logout_all` | `logout_all` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/passkey` | `passkey_sign_in` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/passkey/options` | `passkey_sign_in_options` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/passkeys` | `add_passkey` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/passkeys/options` | `passkey_options` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/password` | `change_password` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/saml/{tenant}/acs` | `saml_acs` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/saml/{tenant}/slo` | `saml_slo` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/sso/finish` | `sso_finish` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/sso/start` | `sso_start` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/sso/{provider}/backchannel-logout` | `sso_backchannel_logout` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up` | `step_up` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/accounts` | `step_up_accounts` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/accounts/finish` | `step_up_accounts_finish` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/passkey` | `step_up_passkey` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/passkey/options` | `step_up_passkey_options` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/provider` | `step_up_provider` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/step-up/provider/finish` | `step_up_provider_finish` | identity |
| `server/src/controllers/auth.rs` | POST | `/api/auth/token` | `page_token` | identity |
| `server/src/controllers/collab.rs` | DELETE | `/api/comments/{id}` | `delete_comment` | collaboration |
| `server/src/controllers/collab.rs` | DELETE | `/api/files/{id}` | `delete_file` | collaboration |
| `server/src/controllers/collab.rs` | PATCH | `/api/comments/{id}` | `edit_comment` | collaboration |
| `server/src/controllers/collab.rs` | POST | `/api/assignments/{id}/close` | `close_assignment` | collaboration |
| `server/src/controllers/collab.rs` | POST | `/api/v1/{entity}/{id}/assignments` | `assign` | collaboration |
| `server/src/controllers/collab.rs` | POST | `/api/v1/{entity}/{id}/comments` | `add_comment` | collaboration |
| `server/src/controllers/collab.rs` | PUT | `/api/comments/{id}/shared` | `share_comment` | collaboration |
| `server/src/controllers/collab.rs` | PUT | `/api/files/{id}/shared` | `share_file` | collaboration |
| `server/src/controllers/collab.rs` | PUT | `/api/v1/{entity}/{id}/follow` | `set_following` | collaboration |
| `server/src/controllers/collab.rs` | PUT | `/api/v1/{entity}/{id}/tags` | `set_tags` | collaboration |
| `server/src/controllers/connect.rs` | DELETE | `/api/admin/connector-instances/{id}` | `disconnect` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/admin/connection-drift/{id}/resolve` | `resolve` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/admin/connector-instances` | `create` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/admin/connector-instances/{id}/reconcile` | `reconcile` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/admin/connector-instances/{id}/sync` | `sync` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/admin/connector-instances/{id}/test` | `test` | administration |
| `server/src/controllers/connect.rs` | POST | `/api/connect/{id}/webhook` | `webhook` | protocol: a connector's webhook |
| `server/src/controllers/connect.rs` | PUT | `/api/admin/connector-instances/{id}` | `update` | administration |
| `server/src/controllers/connections.rs` | DELETE | `/api/admin/connections/{id}` | `remove` | administration |
| `server/src/controllers/connections.rs` | DELETE | `/api/connections/{id}/connect` | `disconnect` | identity |
| `server/src/controllers/connections.rs` | PATCH | `/api/admin/connections/{id}/secrets` | `secrets` | administration |
| `server/src/controllers/connections.rs` | POST | `/api/admin/connections` | `add` | administration |
| `server/src/controllers/connections.rs` | POST | `/api/admin/connections/{id}/pulls/{name}` | `pull_now` | administration |
| `server/src/controllers/connections.rs` | POST | `/api/admin/connections/{id}/session` | `session_now` | administration |
| `server/src/controllers/connections.rs` | POST | `/api/admin/connections/{id}/try` | `try_it` | administration |
| `server/src/controllers/connections.rs` | POST | `/api/connections/inbound/{id}` | `inbound` | protocol: a connection's inbound webhook |
| `server/src/controllers/connections.rs` | POST | `/api/connections/{id}/connect` | `connect` | identity |
| `server/src/controllers/connections.rs` | PUT | `/api/admin/connections/{id}` | `change` | administration |
| `server/src/controllers/connections.rs` | PUT | `/api/admin/connections/{id}/debug` | `debug` | administration |
| `server/src/controllers/dav.rs` | ANY | `/.well-known/caldav` | `well_known` | protocol: DAV |
| `server/src/controllers/dav.rs` | ANY | `/.well-known/carddav` | `well_known` | protocol: DAV |
| `server/src/controllers/dav.rs` | ANY | `/api/dav/{*rest}` | `dav` | protocol: DAV |
| `server/src/controllers/docs.rs` | DELETE | `/api/v1/{entity}/{id}` | `remove` | operation: *.delete |
| `server/src/controllers/docs.rs` | PATCH | `/api/v1/{entity}/{id}` | `update` | operation: *.update |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}` | `create` | operation: *.create |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/_bulk` | `bulk` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/_bulk/jobs` | `bulk_job` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/_actions/{action}` | `collection` | operation: *.{action} |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/_calc` | `calc` | operation: preview_record |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/_import` | `import` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/actions/{action}` | `action` | operation: *.{action} |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/amend` | `amend` | operation: amend_record |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/approval/decide` | `decide` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/approval/recall` | `recall` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/cancel` | `cancel` | operation: *.cancel |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/email` | `email_doc` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/merge` | `merge` | debt |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/submit` | `submit` | operation: *.submit |
| `server/src/controllers/docs.rs` | POST | `/api/v1/{entity}/{id}/workflow/{action}` | `transition` | operation: *.transition.{action} |
| `server/src/controllers/docs.rs` | PUT | `/api/v1/{entity}/{id}` | `update` | operation: *.update |
| `server/src/controllers/documents.rs` | DELETE | `/api/admin/documents/templates/{key}` | `reset` | administration |
| `server/src/controllers/documents.rs` | POST | `/api/admin/documents/preview` | `preview` | administration |
| `server/src/controllers/documents.rs` | POST | `/api/admin/documents/templates` | `save` | administration |
| `server/src/controllers/documents.rs` | POST | `/api/admin/documents/templates/{key}/duplicate` | `duplicate` | administration |
| `server/src/controllers/documents.rs` | POST | `/api/documents/{key}/labels` | `labels` | debt |
| `server/src/controllers/documents.rs` | POST | `/api/documents/{key}/query` | `query` | debt |
| `server/src/controllers/documents.rs` | PUT | `/api/admin/documents/defaults/{entity}` | `set_default` | administration |
| `server/src/controllers/domains.rs` | DELETE | `/api/admin/domains/{host}` | `remove` | administration |
| `server/src/controllers/domains.rs` | PATCH | `/api/admin/domains/{host}` | `reshape` | administration |
| `server/src/controllers/domains.rs` | POST | `/api/admin/domains` | `add` | administration |
| `server/src/controllers/domains.rs` | POST | `/api/admin/domains/{host}/check` | `check` | administration |
| `server/src/controllers/domains.rs` | PUT | `/api/admin/domains/{host}/primary` | `primary` | administration |
| `server/src/controllers/domains.rs` | PUT | `/api/admin/paths` | `put_paths` | administration |
| `server/src/controllers/jobs.rs` | POST | `/api/admin/export` | `export` | administration |
| `server/src/controllers/jobs.rs` | POST | `/api/jobs/{id}/cancel` | `cancel` | debt |
| `server/src/controllers/jobs.rs` | POST | `/api/jobs/{id}/retry` | `retry` | debt |
| `server/src/controllers/links.rs` | DELETE | `/api/links/{id}` | `revoke` | collaboration |
| `server/src/controllers/links.rs` | POST | `/api/public/{tenant}/links/{token}` | `act` | public |
| `server/src/controllers/links.rs` | POST | `/api/v1/{entity}/{id}/links` | `issue` | collaboration |
| `server/src/controllers/mcp.rs` | DELETE | `/mcp` | `not_allowed` | protocol: MCP |
| `server/src/controllers/mcp.rs` | POST | `/mcp` | `post` | protocol: MCP |
| `server/src/controllers/messages.rs` | DELETE | `/api/admin/messages/channels/{id}` | `remove_channel` | administration |
| `server/src/controllers/messages.rs` | DELETE | `/api/admin/messages/templates/{key}` | `reset` | administration |
| `server/src/controllers/messages.rs` | POST | `/api/admin/messages/channels` | `add_channel` | administration |
| `server/src/controllers/messages.rs` | POST | `/api/admin/messages/preview` | `preview` | administration |
| `server/src/controllers/messages.rs` | POST | `/api/admin/messages/templates` | `save` | administration |
| `server/src/controllers/messages.rs` | POST | `/api/admin/messages/test` | `test` | administration |
| `server/src/controllers/messages.rs` | POST | `/api/messages/inbound/{id}` | `inbound` | protocol: a channel's inbound messages |
| `server/src/controllers/messages.rs` | POST | `/api/messages/opt-out` | `opt_out_by_link` | public |
| `server/src/controllers/messages.rs` | PUT | `/api/admin/messages/channels/{id}` | `change_channel` | administration |
| `server/src/controllers/messages.rs` | PUT | `/api/admin/messages/opt-outs` | `opt_out_by_staff` | administration |
| `server/src/controllers/notifications.rs` | DELETE | `/api/push` | `push_off` | collaboration |
| `server/src/controllers/notifications.rs` | POST | `/api/notifications/read-all` | `mark_all_read` | collaboration |
| `server/src/controllers/notifications.rs` | POST | `/api/notifications/{id}/read` | `mark_read` | collaboration |
| `server/src/controllers/notifications.rs` | PUT | `/api/notifications/preferences` | `choose` | collaboration |
| `server/src/controllers/notifications.rs` | PUT | `/api/push` | `push_on` | collaboration |
| `server/src/controllers/oauth.rs` | POST | `/api/oauth/authorize` | `authorize` | identity |
| `server/src/controllers/oauth.rs` | POST | `/oauth/authorize` | `consent_answer` | identity |
| `server/src/controllers/oauth.rs` | POST | `/oauth/register` | `register` | identity |
| `server/src/controllers/oauth.rs` | POST | `/oauth/revoke` | `revoke` | identity |
| `server/src/controllers/oauth.rs` | POST | `/oauth/token` | `token` | identity |
| `server/src/controllers/oidc.rs` | POST | `/api/oauth/authorize/confirm` | `confirm` | identity |
| `server/src/controllers/oidc.rs` | POST | `/oauth/userinfo` | `userinfo` | identity |
| `server/src/controllers/operator.rs` | DELETE | `/api/operator/secrets/{owner}/{name}` | `remove_secret` | administration |
| `server/src/controllers/operator.rs` | DELETE | `/api/operator/settings/{key}` | `unset` | administration |
| `server/src/controllers/operator.rs` | POST | `/api/operator/secrets/{owner}/{name}/rotate` | `set_secret` | administration |
| `server/src/controllers/operator.rs` | PUT | `/api/operator/secrets/{owner}/{name}` | `set_secret` | administration |
| `server/src/controllers/operator.rs` | PUT | `/api/operator/secrets/{owner}/{name}/rule` | `set_rule` | administration |
| `server/src/controllers/operator.rs` | PUT | `/api/operator/settings/{key}` | `set` | administration |
| `server/src/controllers/plugins.rs` | ANY | `/api/ext/{plugin}/{*path}` | `ext` | protocol: a plugin's endpoints, run by the plugin host |
| `server/src/controllers/plugins.rs` | ANY | `/api/public/{tenant}/{plugin}/{*path}` | `public` | public |
| `server/src/controllers/prefs.rs` | DELETE | `/api/prefs/{scope}` | `reset` | collaboration |
| `server/src/controllers/prefs.rs` | DELETE | `/api/views/{entity}/{id}` | `unshare` | collaboration |
| `server/src/controllers/prefs.rs` | POST | `/api/views/{entity}` | `share` | collaboration |
| `server/src/controllers/prefs.rs` | PUT | `/api/prefs/{scope}` | `set` | collaboration |
| `server/src/controllers/prefs.rs` | PUT | `/api/views/{entity}/{id}` | `change` | collaboration |
| `server/src/controllers/scim.rs` | DELETE | `/api/scim/v2/Groups/{id}` | `delete_group` | identity |
| `server/src/controllers/scim.rs` | DELETE | `/api/scim/v2/Users/{id}` | `delete_user` | identity |
| `server/src/controllers/scim.rs` | PATCH | `/api/scim/v2/Groups/{id}` | `patch_group` | identity |
| `server/src/controllers/scim.rs` | PATCH | `/api/scim/v2/Users/{id}` | `patch_user` | identity |
| `server/src/controllers/scim.rs` | POST | `/api/scim/v2/Groups` | `create_group` | identity |
| `server/src/controllers/scim.rs` | POST | `/api/scim/v2/Users` | `create_user` | identity |
| `server/src/controllers/scim.rs` | PUT | `/api/scim/v2/Groups/{id}` | `replace_group` | identity |
| `server/src/controllers/scim.rs` | PUT | `/api/scim/v2/Users/{id}` | `replace_user` | identity |
| `server/src/controllers/secrets.rs` | DELETE | `/api/admin/secrets/{owner}/{name}` | `remove` | administration |
| `server/src/controllers/secrets.rs` | POST | `/api/admin/secrets/{owner}/{name}/rotate` | `rotate` | administration |
| `server/src/controllers/secrets.rs` | PUT | `/api/admin/secrets/{owner}/{name}` | `set` | administration |
| `server/src/controllers/secrets.rs` | PUT | `/api/admin/secrets/{owner}/{name}/rule` | `set_rule` | administration |
| `server/src/controllers/shell.rs` | POST | `/api/shell/app-token` | `app_token` | identity |
| `server/src/controllers/shell.rs` | POST | `/{*path}` | `page_post` | public |
| `server/src/controllers/sites.rs` | ANY | `/api/app/{package}/{*path}` | `app` | protocol: apps' Site routes, each listed by its app below |
| `server/src/controllers/sites.rs` | ANY | `/api/site/{tenant}/{package}/{*path}` | `public` | public |
| `server/src/controllers/system.rs` | DELETE | `/api/files/uploads/{id}` | `cancel_upload` | debt |
| `server/src/controllers/system.rs` | POST | `/api/files` | `upload` | debt |
| `server/src/controllers/system.rs` | POST | `/api/files/uploads` | `start_upload` | debt |
| `server/src/controllers/system.rs` | POST | `/api/files/uploads/{id}/complete` | `complete_upload` | debt |
| `server/src/controllers/system.rs` | POST | `/api/tenants` | `create_tenant` | administration |
| `server/src/controllers/system.rs` | PUT | `/api/files/uploads/{id}/parts/{offset}` | `upload_part` | debt |
| `server/src/controllers/work.rs` | DELETE | `/api/away` | `clear_away` | collaboration |
| `server/src/controllers/work.rs` | POST | `/api/away` | `set_away` | collaboration |
| `sign:src/site.rs` | POST | `["signing", t, "code"]` | `signing::send_code` | public |
| `sign:src/site.rs` | POST | `["signing", t, "decline"]` | `signing::decline` | public |
| `sign:src/site.rs` | POST | `["signing", t, "reassign"]` | `signing::reassign` | public |
| `sign:src/site.rs` | POST | `["signing", t, "sign"]` | `signing::sign` | public |
| `sign:src/site.rs` | POST | `["signing", t, "verify"]` | `signing::confirm_code` | public |

## Actions without a contract

| App | Package | Entity | Action |
|---|---|---|---|
| assets | assets | Asset | split_asset |
| assets | assets | Asset | update_details |
| assets | assets | AssetDisposal | make_sales_invoice |
| assets | assets | Company | setup_asset_accounts |
| bookings | bookings | Booking | cancel_booking |
| bookings | bookings | Booking | cancel_series |
| bookings | bookings | Booking | complete |
| bookings | bookings | Booking | confirm |
| bookings | bookings | Booking | guest_link |
| bookings | bookings | Booking | keep_despite_clash |
| bookings | bookings | Booking | make_sales_invoice |
| bookings | bookings | Booking | mark_no_show |
| bookings | bookings | Booking | repeat |
| bookings | bookings | BookingStaff | calendar_feed |
| bookings | bookings | BookingStaff | rotate_calendar_feed |
| campaigns | campaigns | AudienceImport | run_import |
| campaigns | campaigns | CampaignSettings | rotate_webhook_secret |
| campaigns | campaigns | EmailCampaign | send_seed |
| campaigns | campaigns | EmailCampaign | send_test |
| campaigns | campaigns | JourneyEnrollment | exit_journey |
| campaigns | campaigns | MarketingJourney | activate |
| campaigns | campaigns | MarketingJourney | pause |
| campaigns | campaigns | MarketingJourney | stop |
| campaigns | campaigns | MarketingSegment | preview |
| campaigns | campaigns | SendingDomain | check |
| commerce | commerce | OrderRefund | approve_refund |
| commerce | commerce | OrderRefund | decline |
| commerce | commerce | OrderRefund | mark_refunded |
| commerce | commerce | OrderRefund | post_books |
| commerce | commerce | PaymentException | approve_refund |
| commerce | commerce | PaymentException | resolve |
| commerce | commerce | PaymentRequest | mark_failed |
| commerce | commerce | PaymentRequest | mark_paid |
| commerce | commerce | PaymentRequest | set_gateway_details |
| commerce | commerce | ReturnRequest | approve_return |
| commerce | commerce | ReturnRequest | decline_return |
| commerce | commerce | ReturnRequest | receive_return |
| commerce | commerce | SalesOrder | web_cancel |
| commerce | commerce | SalesOrder | web_invoice |
| commerce | commerce | SalesOrder | web_resend_mail |
| commerce | commerce | SalesOrder | web_ship |
| commerce | commerce | SalesOrder | web_shopper_cancel |
| commerce | commerce | SalesOrder | web_shopper_return |
| commerce | commerce | SalesOrder | web_tracking_link |
| commerce | commerce | SalesOrder | web_update_items |
| commerce | commerce | WebsiteItem | generate_variants |
| comms | comms | CommsSettings | sync_mail |
| comms | comms | Communication | retry |
| comms | comms | EmailAccount | rotate_inbound_token |
| construction | construction | ConstructionContract | complete |
| construction | construction | ConstructionContract | practical_completion |
| construction | construction | Estimate | accept |
| construction | construction | Estimate | decline |
| construction | construction | Estimate | send |
| construction | construction | ProgressClaim | submit_claim |
| construction | construction | SubcontractClaim | submit_claim |
| construction | construction | Variation | submit_variation |
| contracts | contracts | Contract | record_signed_copy |
| contracts | contracts | Contract | renew_contract |
| contracts | contracts | Contract | send_for_signature |
| contracts | contracts | Contract | submit_contract |
| contracts | contracts | Contract | terminate_contract |
| control | control | HostedWorkspace | provision |
| control | control | Partner | issue_statement |
| control | control | RecoveryRequest | approve |
| control | control | RecoveryRequest | complete |
| control | control | RecoveryRequest | refuse |
| control | control | Release | qualify |
| control | control | Release | withdraw |
| control | control | Rollout | halt |
| control | control | Rollout | resume |
| control | control | Rollout | start |
| crm | crm | Activity | complete |
| crm | crm | Contact | enrol_in_sequence |
| crm | crm | Contact | move_to_account |
| crm | crm | ForecastSubmission | adjust_forecast |
| crm | crm | ForecastSubmission | reopen_forecast |
| crm | crm | ForecastSubmission | submit_forecast |
| crm | crm | Lead | assign_by_rules |
| crm | crm | Lead | convert_to_customer |
| crm | crm | Lead | enrol_in_sequence |
| crm | crm | Lead | make_opportunity |
| crm | crm | Lead | undo_conversion |
| crm | crm | Opportunity | make_quotation |
| crm | crm | Opportunity | mark_lost |
| crm | crm | Opportunity | mark_won |
| crm | crm | Opportunity | reopen |
| crm | crm | Quotation | add_rule_needs |
| crm | crm | Quotation | approve_quotation |
| crm | crm | Quotation | mark_accepted |
| crm | crm | Quotation | mark_lost |
| crm | crm | Quotation | mark_rejected |
| crm | crm | Quotation | mark_sent |
| crm | crm | Quotation | send_quotation |
| crm | crm | SequenceEnrolment | resume_enrolment |
| crm | crm | SequenceEnrolment | stop_enrolment |
| crm | crm_erp | Quotation | make_sales_order |
| dam | dam | AssetShare | revoke_share |
| dam | dam | MediaAsset | approve_asset |
| dam | dam | MediaAsset | archive_asset |
| dam | dam | MediaAsset | reject_asset |
| dam | dam | MediaAsset | share_asset |
| dam | dam | MediaAsset | submit_asset |
| docs | docs | DocsTemplate | create_document |
| drive | drive | DriveSpace | activity |
| drive | drive | DriveSpace | add_comment |
| drive | drive | DriveSpace | add_team_member |
| drive | drive | DriveSpace | append_to_document |
| drive | drive | DriveSpace | check_in |
| drive | drive | DriveSpace | check_out |
| drive | drive | DriveSpace | copy |
| drive | drive | DriveSpace | create_document |
| drive | drive | DriveSpace | create_folder |
| drive | drive | DriveSpace | create_from_template |
| drive | drive | DriveSpace | create_link |
| drive | drive | DriveSpace | create_team_folder |
| drive | drive | DriveSpace | delete_comment |
| drive | drive | DriveSpace | delete_forever |
| drive | drive | DriveSpace | delete_template |
| drive | drive | DriveSpace | download |
| drive | drive | DriveSpace | empty_trash |
| drive | drive | DriveSpace | item_info |
| drive | drive | DriveSpace | keep_version |
| drive | drive | DriveSpace | list_comments |
| drive | drive | DriveSpace | list_folder |
| drive | drive | DriveSpace | list_links |
| drive | drive | DriveSpace | list_shares |
| drive | drive | DriveSpace | list_templates |
| drive | drive | DriveSpace | list_trash |
| drive | drive | DriveSpace | list_versions |
| drive | drive | DriveSpace | move |
| drive | drive | DriveSpace | read_document |
| drive | drive | DriveSpace | recent |
| drive | drive | DriveSpace | remove_team_member |
| drive | drive | DriveSpace | rename |
| drive | drive | DriveSpace | request_access |
| drive | drive | DriveSpace | resolve_comment |
| drive | drive | DriveSpace | restore |
| drive | drive | DriveSpace | restore_version |
| drive | drive | DriveSpace | revoke_link |
| drive | drive | DriveSpace | save_as_template |
| drive | drive | DriveSpace | search |
| drive | drive | DriveSpace | set_colour |
| drive | drive | DriveSpace | set_team_quota |
| drive | drive | DriveSpace | share |
| drive | drive | DriveSpace | shared_with_me |
| drive | drive | DriveSpace | star |
| drive | drive | DriveSpace | starred |
| drive | drive | DriveSpace | trash |
| drive | drive | DriveSpace | unshare |
| drive | drive | DriveSpace | unstar |
| drive | drive | DriveSpace | upload |
| drive | drive | DriveSpace | upload_version |
| drive | drive | DriveSpace | usage |
| edge | edge | DeviceDefinition | publish |
| edge | edge | EdgeFlow | activate |
| edge | edge | EdgeFlow | approve |
| edge | edge | EdgeFlow | revise |
| edge | edge | EdgeFlow | send_back |
| edge | edge | EdgeFlow | stop |
| edge | edge | EdgeFlow | submit_for_review |
| edge | edge | EdgeFlow | watch |
| edge | edge | EdgeRollout | pause |
| edge | edge | EdgeRollout | start |
| education | education | FeeSchedule | invoice_fees |
| education | education | FeeSchedule | make_fees |
| education | education | Fees | make_invoice |
| education | education | ReportCard | publish |
| education | education | Student | set_consent |
| education | education | StudentApplicant | admit |
| education | education | StudentApplicant | approve |
| education | education | StudentApplicant | reject |
| erp | accounting | BadDebtWriteOff | record_recovery |
| erp | accounting | Company | post_opening_invoices |
| erp | accounting | Company | recognise_deferred |
| erp | accounting | Company | setup_chart_of_accounts |
| erp | accounting | FiscalYear | close_period |
| erp | accounting | FiscalYear | reopen_period |
| erp | accounting | PaymentEntry | get_outstanding_invoices |
| erp | accounting | PaymentInstrument | bounce |
| erp | accounting | PaymentInstrument | cancel_instrument |
| erp | accounting | PaymentInstrument | clear |
| erp | accounting | PaymentInstrument | deposit |
| erp | accounting | PaymentInstrument | replace |
| erp | accounting | StoredValueAccount | adjust |
| erp | accounting | StoredValueAccount | block |
| erp | accounting | StoredValueAccount | cash_out |
| erp | accounting | StoredValueAccount | new_code |
| erp | accounting | StoredValueAccount | unblock |
| erp | ae | SalesInvoice | ae_pint |
| erp | ae | SalesInvoice | ae_record_peppol |
| erp | ae | SalesInvoice | ae_send_peppol |
| erp | au | SalesInvoice | au_pint |
| erp | au | SalesInvoice | au_record_peppol |
| erp | au | SalesInvoice | au_send_peppol |
| erp | buying | PurchaseInvoice | make_debit_note |
| erp | buying | PurchaseInvoice | make_payment |
| erp | buying | PurchaseInvoice | make_refund |
| erp | buying | PurchaseOrder | close |
| erp | buying | PurchaseOrder | make_purchase_invoice |
| erp | buying | PurchaseOrder | make_purchase_receipt |
| erp | buying | PurchaseOrder | reopen |
| erp | buying | PurchaseOrder | update_items |
| erp | buying | PurchaseReceipt | make_carrier_shipment |
| erp | buying | PurchaseReceipt | make_purchase_invoice |
| erp | buying | PurchaseReceipt | make_purchase_return |
| erp | buying | PurchaseReceipt | return_rejected |
| erp | de | SalesInvoice | de_xrechnung |
| erp | es | SalesInvoice | es_record_aeat |
| erp | eu | Company | eu_setup_vat |
| erp | eu | Company | eu_update_rates |
| erp | eu | Customer | eu_check_vat |
| erp | eu | Item | eu_suggest_rates |
| erp | eu | SalesInvoice | eu_record_peppol |
| erp | eu | SalesInvoice | eu_record_peppol_failure |
| erp | eu | SalesInvoice | eu_send_peppol |
| erp | eu | SalesInvoice | eu_ubl |
| erp | finance | BankTransaction | get_suggestions |
| erp | finance | BankTransaction | make_journal_entry |
| erp | finance | BankTransaction | make_payment_entry |
| erp | finance | BankTransaction | reconcile |
| erp | finance | BankTransaction | unreconcile |
| erp | finance | Dunning | send_letter |
| erp | finance | PaymentRun | make_payments |
| erp | finance | PaymentRun | propose |
| erp | finance | PurchaseInvoice | allocate_advances |
| erp | finance | PurchaseInvoice | fin_hold_payment |
| erp | finance | PurchaseInvoice | fin_release_payment |
| erp | finance | SalesInvoice | allocate_advances |
| erp | finance | Subscription | cancel_subscription |
| erp | finance | Subscription | change_plans |
| erp | finance | Subscription | generate_invoices |
| erp | fr | SalesInvoice | fr_record_pa |
| erp | fr | SalesInvoice | fr_send_pa |
| erp | fr | SalesInvoice | fr_ubl |
| erp | gb | Company | gb_connect_hmrc |
| erp | india | SalesInvoice | cancel_e_invoice |
| erp | india | SalesInvoice | cancel_e_way_bill |
| erp | india | SalesInvoice | e_invoice_json |
| erp | india | SalesInvoice | e_way_bill_json |
| erp | india | SalesInvoice | record_e_invoice |
| erp | india | SalesInvoice | record_e_invoice_cancellation |
| erp | india | SalesInvoice | record_e_invoice_cancellation_failure |
| erp | india | SalesInvoice | record_e_invoice_failure |
| erp | india | SalesInvoice | record_e_way_bill |
| erp | india | SalesInvoice | record_e_way_bill_cancellation |
| erp | india | SalesInvoice | record_e_way_bill_failure |
| erp | india | SalesInvoice | request_e_invoice |
| erp | india | SalesInvoice | request_e_way_bill |
| erp | india | SalesInvoice | set_transport |
| erp | inventory | CarrierShipment | buy_label |
| erp | inventory | CarrierShipment | cancel_label |
| erp | inventory | CarrierShipment | choose_rate |
| erp | inventory | CarrierShipment | get_rates |
| erp | inventory | CarrierShipment | track |
| erp | inventory | StockEntry | receive_transfer |
| erp | it | SalesInvoice | it_fatturapa |
| erp | it | SalesInvoice | it_record_sdi |
| erp | it | SalesInvoice | it_send_sdi |
| erp | jp | SalesInvoice | jp_pint |
| erp | jp | SalesInvoice | jp_record_peppol |
| erp | jp | SalesInvoice | jp_send_peppol |
| erp | manufacturing | Bom | update_cost |
| erp | manufacturing | MrpRun | create_orders |
| erp | manufacturing | MrpRun | run |
| erp | manufacturing | WorkOrder | manufacture |
| erp | manufacturing | WorkOrder | resume |
| erp | manufacturing | WorkOrder | stop |
| erp | mfg_ops | Bom | mfo_update_operating_cost |
| erp | mfg_ops | JobCard | make_material_transfer |
| erp | mfg_ops | ProductionPlan | get_items |
| erp | mfg_ops | ProductionPlan | get_material_requests |
| erp | mfg_ops | ProductionPlan | get_raw_materials |
| erp | mfg_ops | ProductionPlan | get_sales_orders |
| erp | mfg_ops | ProductionPlan | make_material_requests |
| erp | mfg_ops | ProductionPlan | make_work_orders |
| erp | mfg_ops | SubcontractingOrder | make_material_transfer |
| erp | mfg_ops | SubcontractingOrder | make_purchase_order |
| erp | mfg_ops | SubcontractingOrder | make_subcontracting_receipt |
| erp | mfg_ops | SubcontractingOrder | return_excess_materials |
| erp | mfg_ops | WorkOrder | mfo_make_job_cards |
| erp | my | SalesInvoice | my_record_myinvois |
| erp | my | SalesInvoice | my_submit_again |
| erp | nz | SalesInvoice | nz_pint |
| erp | nz | SalesInvoice | nz_record_peppol |
| erp | nz | SalesInvoice | nz_send_peppol |
| erp | pl | SalesInvoice | pl_fa3 |
| erp | pl | SalesInvoice | pl_record_ksef |
| erp | pl | SalesInvoice | pl_send_ksef |
| erp | procurement | MaterialRequest | make_purchase_order |
| erp | procurement | MaterialRequest | make_rfq |
| erp | procurement | MaterialRequest | make_stock_entry |
| erp | procurement | MaterialRequest | make_work_order |
| erp | procurement | MaterialRequest | resume |
| erp | procurement | MaterialRequest | stop |
| erp | procurement | RequestForQuotation | make_supplier_quotation |
| erp | procurement | RequestForQuotation | send |
| erp | procurement | SupplierQuotation | make_purchase_order |
| erp | procurement | SupplierScorecard | refresh |
| erp | ro | Company | ro_connect_anaf |
| erp | ro | SalesInvoice | ro_efactura |
| erp | ro | SalesInvoice | ro_record_anaf |
| erp | ro | SalesInvoice | ro_upload_again |
| erp | ro | SalesInvoice | ro_upload_efactura |
| erp | selling | DeliveryNote | make_carrier_shipment |
| erp | selling | DeliveryNote | make_sales_invoice |
| erp | selling | DeliveryNote | make_sales_return |
| erp | selling | SalesInvoice | make_credit_note |
| erp | selling | SalesInvoice | make_debit_note |
| erp | selling | SalesInvoice | make_inter_company_invoice |
| erp | selling | SalesInvoice | make_payment |
| erp | selling | SalesInvoice | make_refund |
| erp | selling | SalesOrder | close |
| erp | selling | SalesOrder | make_delivery_note |
| erp | selling | SalesOrder | make_sales_invoice |
| erp | selling | SalesOrder | release_stock |
| erp | selling | SalesOrder | reopen |
| erp | selling | SalesOrder | reserve_stock |
| erp | selling | SalesOrder | update_items |
| erp | sg | SalesInvoice | sg_pint |
| erp | sg | SalesInvoice | sg_record_peppol |
| erp | sg | SalesInvoice | sg_send_peppol |
| erp | stock_ops | DeliveryNote | make_packing_slip |
| erp | stock_ops | DeliveryNote | make_quality_inspections |
| erp | stock_ops | Item | make_variants |
| erp | stock_ops | PickList | get_item_locations |
| erp | stock_ops | PickList | make_delivery_note |
| erp | stock_ops | PickList | make_stock_entry |
| erp | stock_ops | PurchaseInvoice | make_quality_inspections |
| erp | stock_ops | PurchaseReceipt | make_quality_inspections |
| erp | stock_ops | SalesInvoice | make_quality_inspections |
| erp | stock_ops | SalesOrder | make_pick_list |
| erp | stock_ops | StockEntry | make_quality_inspections |
| erp | wms | ManagedWarehouse | start_managing |
| erp | wms | PickWave | release |
| field-service | field_service | ServiceAppointment | cancel_appointment |
| field-service | field_service | ServiceRequest | make_work_order |
| field-service | field_service | ServiceRequest | reject |
| field-service | field_service | ServiceRequest | tracking_link |
| field-service | field_service | ServiceWorkOrder | arrive |
| field-service | field_service | ServiceWorkOrder | cancel_work_order |
| field-service | field_service | ServiceWorkOrder | complete |
| field-service | field_service | ServiceWorkOrder | create_invoice |
| field-service | field_service | ServiceWorkOrder | dispatch |
| field-service | field_service | ServiceWorkOrder | start_travel |
| forms | forms | Form | close |
| forms | forms | Form | duplicate |
| forms | forms | Form | prefill_link |
| forms | forms | Form | publish |
| forms | forms | Form | record_response |
| forms | forms | FormResponse | create_record |
| forms | forms | FormResponse | mark_reviewed |
| forms | forms | FormResponse | mark_spam |
| forms | forms | FormResponse | not_spam |
| government | government | Permit | renew |
| government | government | Permit | revoke |
| government | government | PermitApplication | appeal |
| government | government | PermitApplication | approve |
| government | government | PermitApplication | decide_appeal |
| government | government | PermitApplication | issue |
| government | government | PermitApplication | pass_stage |
| government | government | PermitApplication | reject |
| government | government | PermitApplication | submit_application |
| government | government | PermitApplication | take_up |
| government | government | PermitApplication | withdraw |
| grc | grc | Policy | acknowledge |
| grc | grc | Policy | publish |
| grc | grc | Policy | retire |
| grc | grc | RiskAcceptance | accept_risk |
| grc | grc | RiskAcceptance | reject_acceptance |
| grc | grc | RiskAcceptance | revoke_acceptance |
| healthcare | healthcare | Appointment | cancel_appointment |
| healthcare | healthcare | Appointment | check_in |
| healthcare | healthcare | Appointment | complete |
| healthcare | healthcare | Appointment | no_show |
| healthcare | healthcare | InsuranceClaim | reject |
| healthcare | healthcare | PatientConsent | withdraw |
| helpdesk | helpdesk | InboundLog | discard |
| helpdesk | helpdesk | InboundLog | release |
| helpdesk | helpdesk | KbArticle | mark_helpful |
| helpdesk | helpdesk | KbArticle | mark_not_helpful |
| helpdesk | helpdesk | MaintenanceSchedule | make_visit |
| helpdesk | helpdesk | SavedReply | render |
| helpdesk | helpdesk | SavedReply | used |
| helpdesk | helpdesk | Ticket | apply_macro |
| helpdesk | helpdesk | Ticket | approve_outside_entitlement |
| helpdesk | helpdesk | Ticket | decline_outside_entitlement |
| helpdesk | helpdesk | Ticket | make_warranty_claim |
| helpdesk | helpdesk | Ticket | merge_tickets |
| helpdesk | helpdesk | Ticket | rate |
| helpdesk | helpdesk | WarrantyClaim | make_maintenance_visit |
| hospitality | hospitality | HotelProperty | night_audit |
| hospitality | hospitality | HotelProperty | open_business_date |
| hospitality | hospitality | HousekeepingTask | cleaned |
| hospitality | hospitality | HousekeepingTask | inspect |
| hospitality | hospitality | Stay | cancel_stay |
| hospitality | hospitality | Stay | check_in |
| hospitality | hospitality | Stay | check_out |
| hospitality | hospitality | Stay | take_deposit |
| hr | hr | Employee | issue_appointment_letter |
| hr | hr | Employee | issue_employment_certificate |
| hr | hr | Employee | issue_warning_letter |
| hr | hr | Employee | request_letter |
| hr | hr | EmployeeDocument | verify |
| hr | hr | EmployeeOnboarding | apply_checklist |
| hr | hr | EmployeeSeparation | apply_checklist |
| hr | hr | EmployeeSeparation | issue_relieving_letter |
| hr | hr | HolidayList | add_weekly_offs |
| hr | hr | JobOffer | accept |
| hr | hr | JobOffer | issue_offer_letter |
| hr | hr | JobOffer | make_employee |
| hr | hr | JobOffer | reject |
| hr | hr | LeavePolicy | accrue |
| hr | hr | LeavePolicy | assign_allocations |
| hr | hr | ShiftType | mark_attendance |
| hr | hr_erp | Company | setup_hr_accounts |
| hr | hr_erp | EmployeeAdvance | make_payment_entry |
| hr | hr_erp | EmployeeAdvance | make_return_entry |
| hr | hr_erp | EmployeeSeparation | make_full_and_final |
| hr | hr_erp | ExpenseClaim | get_advances |
| hr | hr_erp | ExpenseClaim | make_payment_entry |
| hr | hr_erp | FullAndFinalStatement | get_outstanding |
| hr | hr_erp | FullAndFinalStatement | make_payment_entry |
| hr | payroll | AdditionalSalary | stop_or_change |
| hr | payroll | EmployeeTaxExemptionDeclaration | verify_proofs |
| hr | payroll | PayrollEntry | create_salary_slips |
| hr | payroll | PayrollEntry | get_employees |
| hr | payroll | PayrollEntry | submit_salary_slips |
| hr | payroll | SalarySlip | choose_late_changes |
| hr | payroll | SalarySlip | hold_salary |
| hr | payroll | SalarySlip | mail_payslip |
| hr | payroll | SalarySlip | make_correction |
| hr | payroll | SalarySlip | release_salary |
| hr | payroll | SalaryStructure | set_active |
| hr | payroll | SalaryStructureAssignment | issue_salary_certificate |
| hr | payroll_erp | EmployeeLoan | defer_instalment |
| hr | payroll_erp | EmployeeLoan | repay |
| hr | payroll_erp | EmployeeLoan | write_off |
| hr | payroll_erp | PayrollEntry | bank_file |
| hr | payroll_erp | PayrollEntry | make_bank_entry |
| hr | payroll_erp | PayrollEntry | record_payment_elsewhere |
| hr | payroll_erp | PayrollEntry | undo_payment_elsewhere |
| intelligence | intelligence | Example | accept |
| intranet | intranet | Announcement | acknowledge |
| intranet | intranet | Announcement | archive |
| intranet | intranet | Announcement | publish |
| itsm | itsm | CatalogRequest | approve_request |
| itsm | itsm | CatalogRequest | cancel_request |
| itsm | itsm | CatalogRequest | fulfil |
| itsm | itsm | CatalogRequest | reject_request |
| itsm | itsm | ChangeTemplate | approve_template |
| itsm | itsm | ChangeTemplate | raise_change |
| itsm | itsm | ChangeTemplate | withdraw_template |
| itsm | itsm | Incident | confirm_resolution |
| itsm | itsm | Incident | declare_major |
| itsm | itsm | Incident | end_major |
| itsm | itsm | Incident | major_update |
| itsm | itsm | Incident | reject_resolution |
| itsm | itsm | ItChange | approve_change |
| itsm | itsm | ItChange | close_change |
| itsm | itsm | ItChange | fail_change |
| itsm | itsm | ItChange | implement |
| itsm | itsm | ItChange | reject_change |
| itsm | itsm | ItChange | review_change |
| itsm | itsm | ItChange | submit_change |
| itsm | itsm | Problem | resolve_linked_incidents |
| itsm | itsm | RequestTask | complete_task |
| live | live | Broadcast | call_off |
| live | live | Broadcast | end |
| live | live | Broadcast | issue_publisher_link |
| live | live | Broadcast | issue_viewer_link |
| live | live | Broadcast | revoke_links |
| live | live | BroadcastLink | revoke |
| live | live | LiveSettings | set_signing_key |
| lms | lms | LmsAssignmentSubmission | grade |
| lms | lms | LmsCertificate | revoke |
| lms | lms | LmsCohort | mark_attendance |
| lms | lms | LmsCohort | schedule_sessions |
| lms | lms | LmsCohort | session_attendance |
| lms | lms | LmsCourse | enroll_learners |
| lms | lms | LmsEnrollment | reactivate |
| lms | lms | LmsEnrollment | withdraw |
| lms | lms | LmsEnrollmentRule | preview |
| lms | lms | LmsLearningPath | join |
| lms | lms | LmsPost | close |
| lms | lms | LmsPost | mark_answer |
| lms | lms | LmsPost | pin |
| lms | lms | LmsPost | remove |
| lms | lms | LmsQuizAttempt | grade_answers |
| lms | lms | LmsQuizAttempt | review |
| lms | lms_payments | LmsCourseBundle | buy |
| logistics | logistics | Trip | cancel_trip |
| logistics | logistics | Trip | complete |
| logistics | logistics | Trip | dispatch |
| logistics | logistics | Trip | plan_route |
| logistics | logistics | Trip | record_weight |
| logistics | logistics | Vehicle | serviced |
| metrics | metrics | Dataset | certify |
| metrics | metrics | Dataset | deprecate |
| metrics | metrics | Metric | certify |
| metrics | metrics | Metric | deprecate |
| metrics | metrics | Metric | snapshot |
| nonprofit | nonprofit | Donation | refund |
| nonprofit | nonprofit | Membership | cancel_membership |
| nonprofit | nonprofit | Membership | make_invoice |
| objectives | objectives | Objective | close_objective |
| objectives | objectives | OkrCycle | close_cycle |
| objectives | objectives | OkrCycle | start_cycle |
| pos | pos | PosInvoice | void |
| pos | pos | PosPriceDifference | review |
| pos | pos | PosStockShortfall | resolve |
| projects | projects | ChangeRequest | approve |
| projects | projects | ChangeRequest | reject |
| projects | projects | Project | complete |
| projects | projects | Project | create_from_template |
| projects | projects | Project | save_as_template |
| projects | projects | Task | make_milestone_invoice |
| projects | projects | Timesheet | start_timer |
| projects | projects | Timesheet | stop_timer |
| projects | projects_erp | Project | make_sales_invoice |
| projects | projects_erp | Timesheet | make_sales_invoice |
| property | property | Lease | activate |
| property | property | Lease | give_notice |
| property | property | Lease | return_deposit |
| property | property | Lease | review_rent |
| property | property | Lease | terminate |
| property | property | OwnerStatement | bill_owner |
| property | property | ServiceChargeBudget | activate_budget |
| property | property | ServiceChargeBudget | reconcile |
| restaurant | restaurant | KitchenTicket | ready |
| restaurant | restaurant | KitchenTicket | served |
| restaurant | restaurant | KitchenTicket | start |
| restaurant | restaurant | RestaurantOrder | bill |
| restaurant | restaurant | RestaurantOrder | cancel_order |
| restaurant | restaurant | RestaurantOrder | delivered |
| restaurant | restaurant | RestaurantOrder | dispatch |
| restaurant | restaurant | RestaurantOrder | move_to |
| restaurant | restaurant | RestaurantOrder | send_to_kitchen |
| restaurant | restaurant | RestaurantOrder | settle |
| retail | retail | CollectOrder | bring_from |
| retail | retail | CollectOrder | cancel_collect |
| retail | retail | CollectOrder | hand_over |
| retail | retail | CollectOrder | make_ready |
| retail | retail | RoyaltyReport | invoice_royalty |
| retail | retail | StoreReturn | cancel_return |
| retail | retail | StoreReturn | take_back |
| retail | retail | StoreShipment | cancel_shipment |
| retail | retail | StoreShipment | ship_it |
| retail | retail | StoreTransfer | cancel_transfer |
| retail | retail | StoreTransfer | receive_part |
| retail | retail | StoreTransfer | receive_transfer |
| retail | retail | StoreTransfer | reject_transfer |
| retail | retail | StoreTransfer | send_transfer |
| retail | retail | StoreTransfer | write_off_rest |
| sign | sign | SignRequest | remind |
| sign | sign | SignRequest | resend |
| sign | sign | SignRequest | send |
| sign | sign | SignRequest | verify |
| sign | sign | SignRequest | void |
| sign | sign | SignSettings | rotate_signing_key |
| sign | sign | SignTemplate | create_request |
| social | social | SocialAccount | check |
| social | social | SocialAccount | choose |
| social | social | SocialAccount | connect |
| social | social | SocialAccount | disconnect |
| social | social | SocialAccount | reachable |
| social | social | SocialAccount | rules |
| social | social | SocialBrand | media_from_address |
| social | social | SocialBrand | media_search |
| social | social | SocialBrand | write_new |
| social | social | SocialMessage | reply |
| social | social | SocialPost | adapt |
| social | social | SocialPost | add_to_queue |
| social | social | SocialPost | copies |
| social | social | SocialPost | describe |
| social | social | SocialPost | duplicate |
| social | social | SocialPost | hashtags |
| social | social | SocialPost | picture |
| social | social | SocialPost | publish_now |
| social | social | SocialPost | reschedule |
| social | social | SocialPost | retry |
| social | social | SocialPost | review_link |
| social | social | SocialPost | rewrite |
| social | social | SocialPost | save_as_template |
| social | social | SocialPost | send_for_review |
| social | social | SocialPost | stop_repeat |
| social | social | SocialPost | thread |
| social | social | SocialPost | unschedule |
| social | social | SocialPost | write |
| social | social | SocialPublication | delete_remote |
| social | social | SocialSet | use_set |
| social | social | SocialTemplate | use_template |
| studio | studio | Report | run |
| website | website | WebPage | publish |
| website | website | WebPage | unpublish |
