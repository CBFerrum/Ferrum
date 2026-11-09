-- A held record's evidence survives a change, whoever's SQL makes it (FEO-007). The database already refuses
-- deleting a held record (20261105000006); a change to one is not refused, as work on it goes on, but the row
-- as it was before each change is kept while a legal hold covers the record, so an app's own SQL that blanks a
-- field leaves the held version readable. What is kept goes when the record's last hold is released.
create table held_version (
  tenant_id uuid not null references tenant (id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  saved_at timestamptz not null default clock_timestamp(),
  row jsonb not null
);
create index held_version_record on held_version (tenant_id, entity, record_id, saved_at);
alter table held_version enable row level security;
alter table held_version force row level security;
create policy tenant_isolation on held_version using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- On every entity table the metadata sync makes (it names the entity as the argument), before each update.
create or replace function ferrum_held_keep() returns trigger
language plpgsql as $$
begin
  if exists (
    select 1 from legal_hold h
    where h.tenant_id = old.tenant_id and h.released_at is null and h.entity = tg_argv[0] and h.record_id = old.id
  ) then
    insert into held_version (tenant_id, entity, record_id, row) values (old.tenant_id, tg_argv[0], old.id, to_jsonb(old));
  end if;
  return new;
end $$;
