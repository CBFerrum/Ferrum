-- What Studio's solution and run pages need (crates/studio/src/logic.rs, docs/logic/studio.md).

-- A solution being edited. Saved against the version its editor read: a stale save is refused with what is
-- there now, never merged silently (FLC-UI-006). A draft is not a release: saving one compiles and keeps an
-- immutable release beside it.
create table logic_draft (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  solution text not null default '',
  label text not null default '',
  source text not null check (length(source) <= 1048576),
  version int not null default 1,
  created_by uuid,
  updated_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

alter table logic_draft enable row level security;
alter table logic_draft force row level security;
create policy tenant_isolation on logic_draft
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- The record a run is about, so its page can show the processes holding it.
alter table logic_run add column subject_entity text, add column subject_id uuid;
create index logic_run_subject on logic_run (tenant_id, subject_id) where subject_id is not null;

-- Runs' changes in the order their transactions became visible. A reader keeps the cursor it was given, and a
-- change made by a transaction still open when it read is in a later answer, never skipped (FLC-PROC-014).
alter table logic_run add column changed xid8 not null default pg_current_xact_id();
create function logic_run_changed() returns trigger language plpgsql as $$
begin
  new.changed := pg_current_xact_id();
  return new;
end $$;
create trigger logic_run_changed before update on logic_run for each row execute function logic_run_changed();
create index logic_run_changes on logic_run (tenant_id, changed);
