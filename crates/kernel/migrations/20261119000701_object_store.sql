-- Object storage only (docs/plan/stateless.md, D6; docs/operations.md, "Object storage"). New tables an older build
-- never reads. Every statement is safe to run again.
--
-- `file_store`: where the installation's files are, once a server of this release has seen them in object storage
-- (one row). 'moving': the store takes every write, and reads look there first and in the local directory second,
-- until `ferrum task files_move` has copied the directory and confirmed it. 'store': the directory is no longer read.
-- No row: an installation on a local directory, or one that has only ever used a store.
create table if not exists file_store (
  id boolean primary key default true check (id),
  state text not null check (state in ('moving', 'store')),
  -- When files began to move (a replica started before then may still write to the directory).
  since timestamptz not null default now(),
  confirmed_at timestamptz,
  updated_at timestamptz not null default now()
);

-- Each file of the directory `files_move` has copied to the store and read back the same (its length and SHA-256):
-- a run after a crash, or a second run, skips them.
create table if not exists file_move (
  key text primary key,
  size_bytes bigint not null check (size_bytes >= 0),
  sha256 text not null,
  copied_at timestamptz not null default now()
);

-- Objects whose deletion waits (`files_delete_after_days`), so that a database restored to any moment within the
-- backups' reach finds every file it names. A write of the same key takes it off.
create table if not exists object_release (
  key text primary key,
  due_at timestamptz not null,
  requested_at timestamptz not null default now()
);
create index if not exists object_release_due on object_release (due_at);

-- The whole installation's, not a workspace's: only system transactions see them.
alter table file_store enable row level security;
alter table file_store force row level security;
drop policy if exists system_only on file_store;
create policy system_only on file_store using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
alter table file_move enable row level security;
alter table file_move force row level security;
drop policy if exists system_only on file_move;
create policy system_only on file_move using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
alter table object_release enable row level security;
alter table object_release force row level security;
drop policy if exists system_only on object_release;
create policy system_only on object_release using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
