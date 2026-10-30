-- The app shell (docs/plan/apps-ui.md): browser sessions per device, host names that lead to a
-- workspace, kinds of people, and which interface release each workspace runs.

-- One row per signed-in browser; the session cookie's token names it (`sid`). Revoking a row
-- signs that device out; `app_user.token_version` still signs out every device at once.
-- `actor` is who really acts when someone acts as `user_id` (support access, impersonation);
-- `step_up_at` is when the person last proved themselves again for a sensitive action.
create table user_session (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  actor uuid,
  created_at timestamptz not null default now(),
  last_seen_at timestamptz not null default now(),
  step_up_at timestamptz,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  user_agent text not null default '' check (length(user_agent) <= 300),
  ip text not null default '' check (length(ip) <= 64),
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index user_session_live on user_session (tenant_id, user_id) where revoked_at is null;
create index user_session_expired on user_session (expires_at);
alter table user_session enable row level security;
alter table user_session force row level security;
create policy tenant_isolation on user_session
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- Host names that lead to a workspace: its subdomain of the cluster's base domain, or a custom
-- domain the workspace proved it owns (a TXT record carrying `verify_token`). `target` is what
-- the host serves: `shell` (the shell and every app), `portal` (portal pages only) or
-- `site:{app}` (an app's public site at the root). One primary host per workspace and target
-- carries links and redirects. A released host (renamed or deleted workspace) redirects for 90
-- days, then nobody may claim it for a year. Read before any session exists, so it is a cluster
-- table without row-level security.
create table tenant_host (
  host text primary key check (host ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$' and length(host) <= 253),
  tenant_id uuid not null references tenant(id) on delete cascade,
  kind text not null check (kind in ('subdomain', 'custom')),
  target text not null default 'shell' check (target in ('shell', 'portal') or target ~ '^site:[a-z][a-z0-9-]{1,39}$'),
  is_primary boolean not null default false,
  verify_token text not null,
  verified_at timestamptz,
  checked_at timestamptz,
  failures int not null default 0,
  released_at timestamptz,
  created_at timestamptz not null default now()
);
create index tenant_host_tenant on tenant_host (tenant_id);
create unique index tenant_host_primary on tenant_host (tenant_id, target) where is_primary and released_at is null;

-- Members use the workspace's apps; guests reach only what was shared with them; portal people
-- see the workspace's portal; service accounts have no interface.
alter table app_user add column kind text not null default 'member' check (kind in ('member', 'guest', 'portal', 'service'));

-- Interface releases: one row per (app, digest), the files of which live in object storage
-- under `ui/{app}/{digest}/`. `entry` is the app's index.html split around where the shell goes.
-- Kept for 30 days after no workspace uses them, so open tabs keep loading their chunks after an
-- upgrade.
create table ui_release (
  app text not null check (app ~ '^[a-z][a-z0-9-]{1,39}$'),
  digest text not null check (digest ~ '^[0-9a-f]{64}$'),
  files jsonb not null,
  entry jsonb not null,
  created_at timestamptz not null default now(),
  last_used_at timestamptz not null default now(),
  primary key (app, digest)
);

-- Which release an app version runs. Versions that did not change the interface share one.
create table ui_release_version (
  app text not null,
  version text not null check (length(version) <= 64),
  digest text not null,
  created_at timestamptz not null default now(),
  primary key (app, version, digest),
  foreign key (app, digest) references ui_release (app, digest) on delete cascade
);

-- A workspace pinned to an interface release other than the installed one (rollback, canary).
create table tenant_ui_pin (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  digest text not null,
  pinned_at timestamptz not null default now(),
  pinned_by uuid,
  primary key (tenant_id, app),
  foreign key (app, digest) references ui_release (app, digest)
);
alter table tenant_ui_pin enable row level security;
alter table tenant_ui_pin force row level security;
create policy tenant_isolation on tenant_ui_pin
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
