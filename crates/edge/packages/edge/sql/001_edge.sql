-- What a device reports about itself, and what was asked of it. Attributes change rarely, so there
-- is one row for each, not a history: events are the history.
create table edge_attribute (
  tenant_id uuid not null,
  device_id uuid not null,
  key text not null check (length(key) between 1 and 200),
  value jsonb,
  reported_at timestamptz,
  desired jsonb,
  desired_at timestamptz,
  desired_by text,
  primary key (tenant_id, device_id, key)
);

-- Everything a device said happened. Trimmed by age: anything that must last is a fact or a record.
create table edge_event (
  tenant_id uuid not null,
  id uuid not null,
  device_id uuid not null,
  name text not null,
  value jsonb,
  at timestamptz not null,
  primary key (tenant_id, id)
);
create index edge_event_device on edge_event (tenant_id, device_id, at desc);

-- What each gateway last said about itself: its connections and what it could not start.
create table edge_gateway (
  tenant_id uuid not null,
  device_id uuid not null,
  config_version text not null default '',
  runtime text not null default '',
  links jsonb not null default '[]',
  problems jsonb not null default '[]',
  seen_at timestamptz not null default now(),
  primary key (tenant_id, device_id)
);

-- Heard on a shared connection, and nobody's yet.
create table edge_stranger (
  tenant_id uuid not null,
  gateway_id uuid not null,
  code text not null,
  first_seen_at timestamptz not null default now(),
  last_seen_at timestamptz not null default now(),
  primary key (tenant_id, gateway_id, code)
);

do $$
declare t text;
begin
  foreach t in array array['edge_attribute', 'edge_event', 'edge_gateway', 'edge_stranger'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
