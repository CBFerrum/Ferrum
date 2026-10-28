-- Ferrum kernel schema: platform tables. Business entity tables are generated from metadata.
-- Every tenant-owned table carries tenant_id, has RLS forced on, and is isolated by the
-- per-transaction setting `ferrum.tenant_id`. System jobs set `ferrum.system = on`.

create extension if not exists btree_gist;
create extension if not exists citext;
create extension if not exists pg_trgm;

create or replace function ferrum_tenant() returns uuid
language sql stable parallel safe as $$
  select nullif(current_setting('ferrum.tenant_id', true), '')::uuid
$$;

create or replace function ferrum_is_system() returns boolean
language sql stable parallel safe as $$
  select coalesce(current_setting('ferrum.system', true), '') = 'on'
$$;

create or replace function ferrum_forbid_change() returns trigger
language plpgsql as $$
begin
  raise exception 'ferrum: % on % is not allowed (append-only table)', tg_op, tg_table_name
    using errcode = 'P0001';
end $$;

create table tenant (
  id uuid primary key,
  slug text not null unique check (slug ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
  name text not null check (length(name) between 1 and 200),
  status text not null default 'active' check (status in ('active', 'suspended')),
  meta_version bigint not null default 1,
  settings jsonb not null default '{}',
  created_at timestamptz not null default now()
);

create table app_user (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  email citext not null check (length(email) <= 254 and email ~ '^[^@\s]+@[^@\s]+$'),
  full_name text not null default '' check (length(full_name) <= 200),
  password_hash text,
  is_active boolean not null default true,
  is_admin boolean not null default false,
  token_version int not null default 1,
  failed_logins int not null default 0,
  locked_until timestamptz,
  last_login_at timestamptz,
  settings jsonb not null default '{}',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, email)
);

create table role (
  tenant_id uuid not null references tenant(id) on delete cascade,
  name text not null check (length(name) between 1 and 100),
  description text,
  package text,
  primary key (tenant_id, name)
);

create table user_role (
  tenant_id uuid not null,
  user_id uuid not null,
  role text not null,
  primary key (tenant_id, user_id, role),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade,
  foreign key (tenant_id, role) references role (tenant_id, name) on delete cascade on update cascade
);

create table role_permission (
  tenant_id uuid not null,
  role text not null,
  entity text not null,
  can_read boolean not null default false,
  can_write boolean not null default false,
  can_create boolean not null default false,
  can_delete boolean not null default false,
  can_submit boolean not null default false,
  can_cancel boolean not null default false,
  can_export boolean not null default false,
  can_import boolean not null default false,
  if_owner boolean not null default false,
  read_groups text[] not null default '{}',
  write_groups text[] not null default '{}',
  primary key (tenant_id, role, entity, if_owner),
  foreign key (tenant_id, role) references role (tenant_id, name) on delete cascade on update cascade
);

-- Record-level scopes ("user permissions"): user may only see records linked to these values.
create table user_scope (
  tenant_id uuid not null,
  user_id uuid not null,
  entity text not null,
  value uuid not null,
  is_default boolean not null default false,
  primary key (tenant_id, user_id, entity, value),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

create table api_key (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  name text not null check (length(name) between 1 and 100),
  prefix text not null unique,
  secret_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz,
  revoked_at timestamptz,
  last_used_at timestamptz,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

create table installed_package (
  tenant_id uuid not null references tenant(id) on delete cascade,
  name text not null,
  version text not null,
  installed_at timestamptz not null default now(),
  primary key (tenant_id, name)
);

create table meta_overlay (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  seq bigint generated always as identity,
  entity text not null,
  op jsonb not null,
  is_active boolean not null default true,
  note text,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index meta_overlay_seq on meta_overlay (tenant_id, seq);

create table naming_counter (
  tenant_id uuid not null references tenant(id) on delete cascade,
  key text not null,
  value bigint not null check (value >= 0),
  primary key (tenant_id, key)
);

create table audit_event (
  tenant_id uuid not null,
  id bigint generated always as identity,
  at timestamptz not null default clock_timestamp(),
  actor uuid,
  entity text not null,
  record_id uuid not null,
  action text not null,
  version int,
  diff jsonb,
  request_id text,
  prev_hash bytea,
  hash bytea not null,
  primary key (tenant_id, id)
);
create index audit_event_record on audit_event (tenant_id, entity, record_id, id desc);
create index audit_event_at on audit_event (tenant_id, at);
create trigger audit_event_append_only before update or delete on audit_event
  for each row execute function ferrum_forbid_change();
create trigger audit_event_no_truncate before truncate on audit_event
  for each statement execute function ferrum_forbid_change();

create table audit_anchor (
  tenant_id uuid not null references tenant(id) on delete cascade,
  day date not null,
  events bigint not null,
  root_hash bytea not null,
  prev_root bytea,
  created_at timestamptz not null default now(),
  primary key (tenant_id, day)
);

create table outbox (
  id bigint generated always as identity primary key,
  tenant_id uuid not null,
  topic text not null,
  entity text,
  record_id uuid,
  payload jsonb not null,
  created_at timestamptz not null default now(),
  available_at timestamptz not null default now(),
  attempts int not null default 0,
  last_error text,
  processed_at timestamptz,
  dead boolean not null default false
);
create index outbox_pending on outbox (available_at, id) where processed_at is null and not dead;
create index outbox_tenant on outbox (tenant_id, created_at desc);

create or replace function ferrum_outbox_notify() returns trigger
language plpgsql as $$
begin
  perform pg_notify('ferrum_outbox', '');
  return null;
end $$;
create trigger outbox_notify after insert on outbox
  for each statement execute function ferrum_outbox_notify();

create table idempotency_key (
  tenant_id uuid not null,
  key text not null check (length(key) between 8 and 200),
  request_hash text not null,
  status_code int,
  response jsonb,
  created_at timestamptz not null default now(),
  primary key (tenant_id, key)
);
create index idempotency_key_created on idempotency_key (created_at);

create table webhook (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null,
  url text not null check (url ~ '^https?://'),
  secret text not null,
  topics text[] not null,
  is_active boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

create table webhook_delivery (
  tenant_id uuid not null,
  id uuid not null,
  webhook_id uuid not null,
  outbox_id bigint not null,
  topic text not null,
  payload jsonb not null,
  status text not null default 'pending' check (status in ('pending', 'delivered', 'dead')),
  attempts int not null default 0,
  next_attempt_at timestamptz not null default now(),
  response_code int,
  last_error text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, webhook_id, outbox_id),
  foreign key (tenant_id, webhook_id) references webhook (tenant_id, id) on delete cascade
);
create index webhook_delivery_due on webhook_delivery (next_attempt_at) where status = 'pending';

create table automation (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null,
  entity text not null,
  event text not null check (event in ('created', 'updated', 'submitted', 'cancelled', 'deleted')),
  condition text,
  actions jsonb not null,
  is_active boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

create table automation_run (
  tenant_id uuid not null,
  automation_id uuid not null,
  outbox_id bigint not null,
  status text not null check (status in ('done', 'skipped', 'failed')),
  error text,
  created_at timestamptz not null default now(),
  primary key (tenant_id, automation_id, outbox_id)
);

create table notification (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  title text not null,
  body text,
  entity text,
  record_id uuid,
  read_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index notification_unread on notification (tenant_id, user_id, created_at desc) where read_at is null;

create table file_object (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text,
  record_id uuid,
  file_name text not null,
  content_type text not null,
  size_bytes bigint not null check (size_bytes >= 0),
  storage_key text not null,
  sha256 text not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- Tenant-defined entities live here; every field is in `ext`.
create table custom_record (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  number text,
  docstatus smallint not null default 0 check (docstatus between 0 and 2),
  version int not null default 1 check (version >= 1),
  workflow_state text,
  amended_from uuid,
  created_at timestamptz not null default now(),
  created_by uuid,
  updated_at timestamptz not null default now(),
  updated_by uuid,
  ext jsonb not null default '{}',
  primary key (tenant_id, id)
);
create index custom_record_entity on custom_record (tenant_id, entity, created_at desc);
create unique index custom_record_number on custom_record (tenant_id, entity, number) where number is not null;
create index custom_record_ext on custom_record using gin (ext jsonb_path_ops);

-- Cluster-level bookkeeping (not tenant data, no RLS).
create table cluster_package_sql (
  package text not null,
  file text not null,
  checksum text not null,
  applied_at timestamptz not null default now(),
  primary key (package, file)
);

create table cluster_state (
  key text primary key,
  value text not null,
  updated_at timestamptz not null default now()
);

do $$
declare t text;
begin
  foreach t in array array[
    'app_user', 'role', 'user_role', 'role_permission', 'user_scope', 'api_key', 'installed_package',
    'meta_overlay', 'naming_counter', 'audit_event', 'audit_anchor', 'outbox', 'idempotency_key',
    'webhook', 'webhook_delivery', 'automation', 'automation_run', 'notification', 'file_object',
    'custom_record'
  ] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;

alter table tenant enable row level security;
alter table tenant force row level security;
create policy tenant_self on tenant
  using (id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check ((select ferrum_is_system()) or id = (select ferrum_tenant()));
