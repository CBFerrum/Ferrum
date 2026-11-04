-- Licences of apps sold in a store (ferrum-store's GET /v1/licenses/{license}): the one a workspace's
-- administrator gave for each app, and what the store last said of it. A sold app installs or
-- upgrades only with a live one.
create table app_licence (
  tenant_id uuid not null references tenant (id) on delete cascade,
  app_id text not null,
  store text not null,
  licence uuid not null,
  live boolean not null,
  period_end timestamptz,
  checked_at timestamptz not null default now(),
  last_error text,
  primary key (tenant_id, app_id)
);
alter table app_licence enable row level security;
alter table app_licence force row level security;
create policy tenant_isolation on app_licence using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
