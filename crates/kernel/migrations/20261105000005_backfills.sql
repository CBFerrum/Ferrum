-- Backfills packages declare (FERRUM-PLATFORM-010): one row per workspace and backfill, from when it
-- started. The last row a batch reached and how many it changed are written with the batch, so a
-- restart picks up there; when it is done, how many rows still needed it.
create table backfill_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  package text not null,
  name text not null,
  reached uuid,
  changed bigint not null default 0,
  remaining bigint,
  started_at timestamptz not null default now(),
  finished_at timestamptz,
  primary key (tenant_id, package, name)
);

alter table backfill_run enable row level security;
alter table backfill_run force row level security;
create policy tenant_isolation on backfill_run using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
