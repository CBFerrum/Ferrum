-- Each erasure's journal entry, by ids and field names only, as it also goes to object storage
-- (crates/kernel/src/privacy.rs). A workspace taken back to a restored copy (extract_tenant) keeps this
-- table as it is now and erases again everything it names before anyone sees the copy, so an erasure
-- since the copy stays done without a separate task (FEO-078). A whole database restored from a backup
-- loses the rows written after it; the object-storage journal and `ferrum task reapply_erasures` cover that.
create table privacy_erasure (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id bigint generated always as identity,
  erasure jsonb not null,
  erased_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

alter table privacy_erasure enable row level security;
alter table privacy_erasure force row level security;
create policy tenant_isolation on privacy_erasure
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
