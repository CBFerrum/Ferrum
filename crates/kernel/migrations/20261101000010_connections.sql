-- Connections to outside systems (crates/kernel/src/connect.rs). `sealed` holds credentials, bound to
-- the connection by the seal; it is never read back through any API.
create table connection (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  provider text not null check (length(provider) between 1 and 100),
  name text not null check (length(name) between 1 and 100),
  config jsonb not null default '{}',
  mapping jsonb not null default '{}',
  sealed text,
  enabled boolean not null default false,
  cursors jsonb not null default '{}',
  last_success_at timestamptz,
  last_checked_at timestamptz,
  last_error text,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create unique index connection_by_id on connection (id);

-- A record's identity at a provider, both ways, and what it last looked like.
create table connection_link (
  tenant_id uuid not null,
  connection_id uuid not null,
  stream text not null,
  external_id text not null,
  entity text not null,
  record_id uuid not null,
  external_version text,
  hash text,
  local_version int,
  synced_at timestamptz not null default now(),
  primary key (tenant_id, connection_id, stream, external_id),
  foreign key (tenant_id, connection_id) references connection (tenant_id, id) on delete cascade
);
create unique index connection_link_record on connection_link (tenant_id, connection_id, stream, record_id);

-- Webhook events seen, to tell a repeat from a new one.
create table connection_event (
  tenant_id uuid not null,
  connection_id uuid not null,
  event_id text not null,
  received_at timestamptz not null default now(),
  primary key (tenant_id, connection_id, event_id),
  foreign key (tenant_id, connection_id) references connection (tenant_id, id) on delete cascade
);
create index connection_event_received on connection_event (received_at);

-- What synchronization or reconciliation left for a person.
create table connection_drift (
  tenant_id uuid not null,
  id uuid not null,
  connection_id uuid not null,
  stream text not null,
  external_id text not null,
  kind text not null check (kind in ('conflict', 'refused', 'differs', 'missing_here', 'missing_there')),
  detail jsonb not null default '{}',
  created_at timestamptz not null default now(),
  resolved_at timestamptz,
  resolved_by uuid,
  primary key (tenant_id, id),
  foreign key (tenant_id, connection_id) references connection (tenant_id, id) on delete cascade
);
create unique index connection_drift_open on connection_drift (tenant_id, connection_id, stream, external_id, kind) where resolved_at is null;

do $$
declare t text;
begin
  foreach t in array array['connection', 'connection_link', 'connection_event', 'connection_drift'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
