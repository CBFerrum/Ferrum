-- Plugins as platforms: a manifest (routes, subscriptions, schedules, sockets, permissions, outbound
-- hosts, entities), admin-set configuration, a service user to act as, private key-value storage,
-- deduplicated event delivery and multi-replica-safe schedules.
alter table plugin add column manifest jsonb not null default '{}' check (jsonb_typeof(manifest) = 'object');
alter table plugin add column config jsonb not null default '{}' check (jsonb_typeof(config) = 'object');
alter table plugin add column user_id uuid;

create table plugin_kv (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  key text not null check (length(key) between 1 and 200),
  value jsonb not null,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, plugin_id, key),
  foreign key (tenant_id, plugin_id) references plugin (tenant_id, id) on delete cascade,
  check (pg_column_size(value) <= 262144)
);

create table plugin_event_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  outbox_id bigint not null,
  ran_at timestamptz not null default now(),
  primary key (tenant_id, plugin_id, outbox_id),
  foreign key (tenant_id, plugin_id) references plugin (tenant_id, id) on delete cascade
);

create table plugin_schedule (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  name text not null,
  every_secs int not null check (every_secs >= 60),
  next_run_at timestamptz not null default now(),
  last_run_at timestamptz,
  last_error text,
  primary key (tenant_id, plugin_id, name),
  foreign key (tenant_id, plugin_id) references plugin (tenant_id, id) on delete cascade
);
create index plugin_schedule_due on plugin_schedule (next_run_at);

do $$
declare t text;
begin
  foreach t in array array['plugin_kv', 'plugin_event_run', 'plugin_schedule'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
