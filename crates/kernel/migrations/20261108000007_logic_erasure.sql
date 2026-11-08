-- What an erasure took from the copies runs keep of a record (FLC-OPS-006): the event that started a run about
-- it, the run's own record and the material its people were shown lose those fields, and this row stays so a
-- runtime reporting the run again, from a history taken before the erasure, cannot bring them back.
create table logic_erasure (
  tenant_id uuid not null references tenant (id) on delete cascade,
  subject_entity text not null check (length(subject_entity) between 1 and 200),
  subject_id uuid not null,
  fields text[] not null,
  erased_at timestamptz not null default now(),
  primary key (tenant_id, subject_entity, subject_id)
);
alter table logic_erasure enable row level security;
alter table logic_erasure force row level security;
create policy tenant_isolation on logic_erasure
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
