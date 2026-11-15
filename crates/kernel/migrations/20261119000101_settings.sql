-- The server's settings in the database (docs/plan/stateless.md, D2). Made once and never reshaped: an older or newer
-- build reads and writes these columns as they are.
--
-- `setting`: one row per setting and scope (`installation`, or `pool` with the pool's name), holding the text the
-- environment variable would hold, as a JSON string. A row that was unset keeps its place with a null value, so that
-- every change moves `version` (one sequence for the table): a replica that compares the highest version with its own
-- notices a change whose notice it missed.
create sequence setting_version;

create table setting (
  scope text not null check (length(scope) between 1 and 40),
  scope_name text not null default '' check (length(scope_name) <= 63),
  key text not null check (length(key) between 1 and 63),
  value jsonb,
  version bigint not null default nextval('setting_version'),
  updated_at timestamptz not null default now(),
  updated_by text not null check (length(updated_by) between 1 and 200),
  primary key (scope, scope_name, key)
);

-- Every change of a setting, with the value before and after: the operator's record of them, which no workspace sees.
create table setting_change (
  id bigint generated always as identity primary key,
  at timestamptz not null default now(),
  scope text not null,
  scope_name text not null,
  key text not null,
  old_value jsonb,
  new_value jsonb,
  version bigint not null,
  updated_by text not null
);
create trigger setting_change_append_only before update or delete on setting_change
  for each row execute function ferrum_forbid_change();
create trigger setting_change_no_truncate before truncate on setting_change
  for each statement execute function ferrum_forbid_change();

-- The replicas running now: each refreshes its row every 30 seconds, and a row not seen for two minutes is gone.
-- `settings_version` is the settings' version the replica started with: a setting that applies at start and changed
-- after it is one it still runs the older value of.
create table replica (
  id uuid primary key,
  pool text not null check (length(pool) between 1 and 63),
  build_date timestamptz not null,
  started_at timestamptz not null default now(),
  seen_at timestamptz not null default now(),
  settings_version bigint not null default 0
);
create index replica_pool on replica (pool, seen_at);

-- The whole installation's, not a workspace's: only system transactions see them.
alter table setting enable row level security;
alter table setting force row level security;
create policy system_only on setting using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
alter table setting_change enable row level security;
alter table setting_change force row level security;
create policy system_only on setting_change using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
alter table replica enable row level security;
alter table replica force row level security;
create policy system_only on replica using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
