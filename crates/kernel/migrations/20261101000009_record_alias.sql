-- A record merged into another (crates/kernel/src/merge.rs). Its row stays: an append-only ledger may
-- name it for ever, by a foreign key. This says what it became.
create table record_alias (
  tenant_id uuid not null references tenant (id) on delete cascade,
  entity text not null,
  alias_id uuid not null,
  canonical_id uuid not null,
  reason text not null check (length(reason) between 1 and 500),
  merged_by uuid,
  merged_at timestamptz not null default now(),
  primary key (tenant_id, entity, alias_id)
);
create index record_alias_canonical on record_alias (tenant_id, entity, canonical_id);

alter table record_alias enable row level security;
alter table record_alias force row level security;
create policy tenant_isolation on record_alias
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
