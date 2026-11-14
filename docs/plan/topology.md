# Deployment shapes: pools, zones, stable names, moves

Status: being built (started 14 November 2026). Background and evidence: the source reading in
`ENTERPRISE-TOPOLOGY-2026-11-14.md` (kept outside the repository). This file is the decision record; where
the two differ, this file wins.

## Words

- **Pool**: one Deployment of the server image. Pools differ only in settings. Every pool holds every app.
- **Zone**: a named place a request arrives in, with a rule for who is admitted there.
- **Cell**: one whole installation (database, files, keys, pools).
- **Staff**: accounts of kind `member` or `support`, administrators included. **Outside people**: `portal`,
  `guest`. **Machines**: `service`.

## Rules that every part keeps

1. A pool has no state of its own. The state is the database, the files and the keys.
2. Three names do not change when anything else moves: the accounts host (sign-in, passkeys, SSO callbacks),
   the callbacks host (webhooks in), the link host (every emailed or printed link).
3. The previous build starts and runs on the next build's database.
4. Every new setting has a default that keeps an existing installation behaving exactly as before.
5. A new setting reaches the container only if it is added to `server/config/production.yaml`,
   `deploy/docker-compose.yml`, `deploy/k8s/deployment.yaml` and the chart's env helper, and documented in
   `docs/operations.md`.
6. Nothing a client sends decides a zone, except its network address as the configured proxies report it.

## D1. Stable names (phase 0)

| Setting | Meaning | Default |
|---|---|---|
| `FERRUM_ACCOUNTS_HOST` | Host where identities sign in | `accounts.{FERRUM_BASE_DOMAIN}` when a base is set, else none |
| `FERRUM_HOOKS_HOST` | Host for callbacks from other services | `hooks.{FERRUM_BASE_DOMAIN}` when a base is set, else the `PUBLIC_URL` host |
| `FERRUM_LINK_HOST` | Host that every emailed, notified or printed link goes through | none (links as today) |
| `FERRUM_PASSKEY_RP` | WebAuthn relying party | the request host (as today) |

- `FERRUM_ACCOUNTS_HOST` works without a base domain. When it is set, every host of every workspace signs in
  there through the existing `/authorize` → `/_auth/callback` handoff, so the identity provider knows one
  redirect address and a person signs in once.
- `FERRUM_PASSKEY_RP`: used when the request host equals it or is a subdomain of it; otherwise the request
  host. A credential is always verified against the relying party it was registered with, so existing
  passkeys keep working on the host they were made on. On a shared base domain where workspaces are
  subdomains, never set it to the base: leave the accounts host as the relying party.
- Links: with `FERRUM_LINK_HOST` set, links are `https://{link host}/go/{workspace}/{s|p}/…` (`s` for staff,
  `p` for outside people). `/go/` is answered on the link host whatever else that host is, needs no session,
  and redirects (302) to the workspace's current host for that audience (primary `shell` host, or `portal`
  host), keeping the rest of the path, which is the existing `/open/…` form. Unknown workspace: 404.
- Apps that keep their own public address (Sign, Bookings, Comms, LMS) take it from one kernel function
  instead. Follows after the platform part.
- Not changed: the OAuth issuer for MCP clients stays the host they connected to (clients re-add after a host
  move; their tokens keep working).

## D2. Two-version window (phase 0)

- Every build carries its own date (the commit's, embedded at build time; a fixed fallback when there is no
  git) and a hand-kept constant `COMPATIBLE_FROM`: the date of the oldest build that may run on this build's
  schema.
- The build that migrates or syncs the schema records both in `cluster_state`.
- A build that finds the database ahead of it (migrations it does not know, or a schema synced for a newer
  catalog) starts when its own date is not before the recorded `COMPATIBLE_FROM`, and then runs as a **guest**:
  it does not migrate, does not sync the schema, and does none of the start-up writes that would put older
  things back (compiled app and interface registration, upgrade steps). Otherwise it refuses as today, saying
  which build to start.
- A build that finds the database behind it still refuses when `FERRUM_MIGRATE_AT_START=false` (as today).
- A removal or any change an older build cannot live with ships at least one release after the change that
  made it unnecessary, and moves `COMPATIBLE_FROM` in the same commit. `scripts/` gets a check that fails when
  a new migration removes, renames or tightens something and `COMPATIBLE_FROM` did not move.
- The schema sync from the catalog must stay additive between builds in the window: say in the report what it
  can do that is not (a new required column without a default, a changed type, a dropped column) and make
  each either impossible or covered by the check.
- **Floors (14 November 2026).** A rollback must never silently drop a protection that is switched on. Each
  protection an older build in the window does not enforce (zones levels 1 and 2, staff networks, key networks,
  the idle timeout, break-glass, a planned move's fence, a moving workspace's freeze, sandboxes) has a floor: a
  hand-kept date next to the feature, the committer date of the merge commit on main that first enforced it.
  Images are built from main, and a build's date is its commit's, so the merge is the first build that enforces
  it; the date of the branch's own commit would be earlier and would let in main builds made between it and the
  merge, which do not. `window::require` puts it in `window_floor` where the protection is switched on; temporary
  ones (moves, sandboxes) are held per holder and go with the last (`window::release`). The floors act through the
  value every window-aware build already reads: the recorded `schema_compatible_from` is the later of the syncing
  build's own `COMPATIBLE_FROM` (kept as `schema_compatible_from_own`) and the latest floor, and only `record`,
  `require`, `release` and `allow_older_builds` move it, under one transaction lock. A build never writes a floor
  later than itself, and a guest may raise a floor but never release or lower one. Every start of a level build
  puts floors in place for what is in use, so `allow_older_builds` lasts until the next such start. Turning a
  protection off does not lower its floor; `ferrum task allow_older_builds` is the deliberate way down. Only
  starts are refused: replicas already running are not stopped.

## D3. Pools (phase 1, chart)

- `pools:` in values. The first, default pool is today's Deployment with today's names and selector
  (`app.kubernetes.io/name: {fullname}`), so an existing release upgrades in place. Every other pool is named
  `{fullname}-{pool}` and labelled `app.kubernetes.io/name: {fullname}-{pool}`. Each pool also carries
  `ferrum.dev/pool` and `ferrum.dev/zone` on its pods (not in any selector).
- Per pool: `background` (`both` | `off` | `only`; `only` has no Service and no routes), `zone`, replicas,
  autoscaling, resources, `dbMaxConnections`, env, node selector, tolerations, priority class, disruption
  budget, routes (hostnames, path prefixes, apps matched on the `x-ferrum-ui` header), and which gateway.
- A second, private Gateway beside the public one. A NetworkPolicy per pool admits only that pool's gateway.
- The chart refuses to render: a pool whose zone admits no staff with background work on; a total connection
  budget above the database's limit, summed over every pool's maximum replicas plus the migrate Job;
  two pools with one name.
- Presets as values files: `single`, `split` (web and worker), `pools`, `zones`.
- The default render stays byte-comparable with `deploy/k8s` (`deploy/helm/test.sh`).

## D4. Zones (phases 2, 3, 6)

Settings:

| Setting | Meaning | Default |
|---|---|---|
| `FERRUM_ZONES` | JSON: zone name → rule | none: one unnamed zone admitting everyone |
| `FERRUM_ZONE` | The zone this replica is pinned to. A pinned replica is in that zone for every request | none |
| `FERRUM_ZONE_DEFAULT` | Zone of a request no host or network places (unpinned replicas only) | none: admit everyone |
| `FERRUM_ZONE_MODE` | `enforce` or `report` (admit, but count and log what would be refused) | `enforce` |

A rule: `{"staff": "all" | "none" | ["app", …], "admin": true|false, "scim": true|false, "networks": ["cidr", …]}`.
`staff` defaults to `all`; `admin` defaults to true when `staff` is `all`, else false; `scim` defaults to
false; `networks` (unpinned replicas only) places a caller in this zone by address.

Which zone a request is in, in order: the replica's pin; else the zone on the request's host row
(`tenant_host.zone`); else the first zone whose `networks` hold the caller's address; else
`FERRUM_ZONE_DEFAULT`.

**Level 1 (`staff: none`).**
- Staff accounts are refused there on every kind of credential, with one generic refusal that names the
  staff address when the workspace has one. The check sits where a request's context is built, after the
  per-person cache, and it is given the request's zone explicitly: request paths may not build a context
  without one (a test scans for it). Work that has no request behind it (jobs, schedules, tasks) is "inside"
  and is not refused at this level.
- A staff sign-in attempt there fails the same way whatever the password, before the password is checked,
  and does not count toward lockout.
- Portal, guest and service accounts, signed inbound calls, public pages, published lists and action links
  work as everywhere. A SCIM key works only where the rule says `scim: true`.
- A replica pinned to such a zone refuses to start with background work on.
- Pages: a staff page asked for in such a zone answers with a plain page naming the workspace's staff
  address (a `shell` host whose zone admits staff), never a sign-in form for staff.

**Level 2 (`staff: [apps]`).** Staff are admitted, limited to the listed apps' packages and the packages
those depend on; never administrators unless `admin: true`. Needs, beyond narrowing rights: the zone on the
context, carried into "as the system" contexts and into jobs and events; configured effects and `invoke`
refusing hidden packages; approvals requiring read; notifications, mentions and live pushes filtered by
readable entity; role names filtered with rights; the signed-in app endpoint of a hidden package refused
(public endpoints untouched); a test that tries every route and every kind of credential.
Built (14 November 2026) with two changes. A rule may say `custom: true` (only with a list): the workspace's own
entities are seen there; without it they are hidden. "Approvals requiring read" became approvals requiring the
entity be seen in the decider's zone: BD-19 lets a named approver without a role on the entity decide (and read
the one record awaiting them), and that stays.

**Workspace staff networks (phase 3).** A workspace setting: the networks staff may work from, with its own
`report`/`enforce`. Checked at the same place, in addition to the zone. Saving it is refused unless the
saver's own address is inside the new list; `ferrum task` can clear it.

**Safeguards.** The zone is recorded on each session. `GET /_zone` on any host answers the zone name, its
`staff` rule and the caller address the server sees (no secrets): the doctor uses it to check that a public
address is answered by a zone that admits no staff, and that a forged `X-Forwarded-For` is not believed.
Refusals are counted in a metric by zone and mode.

**Hosts.** `tenant_host.zone` (nullable). An operator can add a host the server cannot verify from the
internet (`kind = 'operator'`, never checked, served as verified) with a task.

## D5. Moves (phases 4, 7)

- The chart can start its database from an existing one (import, physical copy, or a backup) as well as empty.
- A planned move: a task fences the source database and leaves a mark in it; the copy that starts elsewhere
  finds the mark, adopts the new database identity without the restore hold, and clears the mark. The source
  stays fenced.
- The doctor reports sealed values no key opens and files the database names that storage lacks.
- `move_workspace` (phase 7): freeze the workspace so that people, system work and background work all stop
  writing it; copy it to another cell with the same id, including what a restore leaves behind; verify;
  release it there; leave a forwarding mark at the source. Copy mode makes a sandbox under a new id with
  secrets blanked and everything that reaches outside held.

## D6. Scale (phase 5)

- `FERRUM_DATABASE_DIRECT_URL`: used for LISTEN, session locks and migrations, so `DATABASE_URL` may point at
  PgBouncer in transaction mode. Default: the same as `DATABASE_URL`.
- `FERRUM_DATABASE_READ_URL`: reports and exports read there when its replay lag is under a limit; otherwise
  the primary. Default: none.
- Worker pool scaling on queue depth through KEDA's PostgreSQL scaler (chart values, no server change).

## D7. Addresses (built 14 November 2026)

A workspace shapes its own addresses without an origin per app: an app on an origin of its own loses what people rely on
across apps (record cards from other apps, side panels, links between apps, one live connection, writes refused across
hosts). So every address that opens the workspace (`shell`) is the whole workspace, and an address only chooses where a
person lands and what the bar lists. Zones stay the operator's: nothing here admits anyone anywhere.

**What an address shows** (`tenant_host.home`, `apps`, `label`; a `shell` address only, refused on `portal` and
`site:`). `home`: `/` redirects (`302`) to that app's first launcher route the person may use, else Home. `apps`: the
launcher and the bar's search list those apps and an entry to the main address; presentation, never a wall. `label`:
beside the workspace's name in the bar. The bootstrap carries `host.home`, `host.apps`, `host.label`, `host.main` and
`host.main_url`, each only as far as the person may open it there: an app a zone leaves out is never named, and
`home` to one falls back to Home. The runtime filters in a pure helper (`addressEntries`). Not filtered: the Create
menu and side panels (actions, not navigation), records search's "Go to" lists (entities, not apps).

**Names under the base**: `{name}--{slug}.{base}`, one DNS label, kind `subdomain` with `base_name` set, live at once,
covered by the wildcard, answered by `/_tls/ask`, handed sign-ins by the accounts host. New slugs may not hold `--`
(`check_slug`); older slugs that do keep working, and the host table's primary key keeps any clash out (first
wins). Reserved: `accounts`, `hooks`, `www`, `api`, `admin`, `login`, `mail`, `xn`, the slugs the cluster keeps, and the
first label of the server's own hosts under the base. A name released by its workspace may be taken back by it at once,
leaving period or not: only it can ever form it.

**Apps' own origins under the same base** (`FERRUM_APPS_BASE` equal to `FERRUM_BASE_DOMAIN`, which the docs advise
against but nothing forbids): the two forms are the same. One place decides (`Shell::app_host`, used by the middleware,
the app origin's pages, the isolated page and OAuth through `Shell::app_origin_for`): a host that is a workspace's
address is the workspace's, and the app keeps to its sandboxed frame. Adding a name that is the id of an app compiled in,
available on the cluster or uploaded to the workspace is refused there. When the database cannot be asked, the host is
kept to the app's paths (closed).

**Short paths** (`tenant.settings.paths`, `[{alias, target}]`, at most 50): `/{alias}[/…]` redirects (`302`, query
kept, rest appended) on every `shell` address, signed in. Asked only where no app's route is, so an app that comes later
wins (the doctor's `paths.shadowed`, `works: false` in the list). Targets: `/`, the shell's pages, `/open/…`, or under a
route of one of the workspace's apps; the joined location must pass `safe_next`. Settings, so they travel with a move and a
sandbox. Read from the database on each such request (no cache): only requests no app answers pay for it.

**Admin API**: `GET/POST /api/admin/domains`, `PATCH|DELETE /api/admin/domains/{host}`, the existing `…/primary` and
`…/check`, `GET|PUT /api/admin/paths`. Operator addresses and zones are shown read-only; naming a zone is refused, and
changing or removing an operator's address is `403`. Changes are refused while the workspace is read-only (closed or
restricted) and are audited (`WorkspaceDomain` `add`/`shape`/`remove`, `WorkspacePaths` `set`).

**Limits**: the plan quota `addresses` (live addresses of every kind; 20 when left out, today's fixed number). An
advisory lock per workspace makes two adds at once take turns, so the last place is taken once; `PATCH` locks the row,
so two changes each build on the other; `PUT` of short paths replaces the list (the later save stands whole).

**Lifecycle.** A move: the target re-makes each live name under its own base, carrying what it shows (taking back one it
had let go of when the workspace left it), and releases the copies under the source's base (they cannot reach it); a
portal or site whose main address was such a name gets the new one. A sandbox: no addresses travel (as before); short
paths do. A rename (by hand: no task exists): names keep their form and keep leading to the workspace; the retired slug
keeps anyone else from forming them. Closed or restricted: addresses keep answering and do not change. Deleted: rows go
with the workspace; names can never be formed again.

**Older builds** (the two-version window): the new columns and the `paths` setting are ignored, so an address lands on
Home and lists every app, and short paths answer "Nothing is installed here". Nothing protective is lost, so no floor.
A name under a base equal to the apps base is taken there for an app's origin and answers "Nothing is here" (closed).

**Drawbacks kept** (each with what was done):

| Drawback | What was done |
|---|---|
| Each address is its own origin: its own storage, offline queue, service worker and push subscription, and a person with two open has two live connections | Said in operations.md; the main address stays the one mail and notifications use |
| A person signs in on each address | With an accounts host it is one redirect with no prompt; without one, a password per address, as for custom domains |
| `home` to an app the person cannot open there | Falls back to Home, decided per request; the bootstrap names it only when it opens |
| `apps` leaves out the app a link or notification opens | The page opens anyway (not a wall); the launcher's entry leads to the main address for the rest |
| Bookmarks to a short path always redirect | On purpose: the address bar shows the real path and nothing can hide an app |
| An app later claims a short path's segment | The app wins; `works: false` and the doctor's `paths.shadowed` say so |
| Name squatting under a shared base | Impossible across workspaces (the form ends in the slug); within one, the `addresses` quota |
| A label or name that looks like another company's | The host always ends in the workspace's own slug, and a `label` shows only in that workspace's bar |
| One certificate per name with on-demand TLS (authorities' weekly limits per domain) | One label under the base, so the wildcard covers it; operations.md says to prefer the wildcard |
| Search engines | `shell` pages need sign-in; a `site:{app}` reached at two addresses shows the same pages twice: give a site one address |
| Removing a name by mistake | Its workspace takes it back at once |
| An administrator in a zone that leaves apps out replaces an address's `apps` | They see and set only the zone's apps; the list they save replaces the address's |
| Renaming a workspace leaves its names in the old slug's form | They keep working and no one else can form them; no rename task exists to re-make them |
| A deleted workspace's custom domains are deleted with it (no quarantine) | Found, not changed here: names under the base are safe by their form, custom domains are not |

## Not doing

A service or database per app; a database role per pool; an image per app group; one host per app; a
service mesh as a requirement.
