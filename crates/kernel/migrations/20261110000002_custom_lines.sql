-- Custom entities that hold a business document (docs/builder/decisions.md BD-06): lines and unique keys,
-- by configuration, with no table per workspace.
--
-- Lines: a custom entity's table rows are custom records of its child entity, under their parent.
-- Each custom child entity is the lines of one table field, so its name tells one table's rows from another's.
alter table custom_record add column parent_id uuid;
alter table custom_record add column idx int;
alter table custom_record add constraint custom_record_parent
  foreign key (tenant_id, parent_id) references custom_record (tenant_id, id) on delete cascade;
create index custom_record_lines on custom_record (tenant_id, parent_id, idx) where parent_id is not null;

-- Unique keys: one row per value a record holds of one of its entity's keys (`key` names the fields,
-- `value` is their values as a JSON array). The primary key enforces uniqueness; empty values and
-- cancelled documents hold none.
create table custom_key (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  key text not null,
  value text not null,
  record_id uuid not null,
  primary key (tenant_id, entity, key, value),
  foreign key (tenant_id, record_id) references custom_record (tenant_id, id) on delete cascade
);
create index custom_key_record on custom_key (tenant_id, record_id);

alter table custom_key enable row level security;
alter table custom_key force row level security;
create policy tenant_isolation on custom_key using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
