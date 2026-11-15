-- The installation's own record of its vault (crates/kernel/src/vault): its slots, its installation key, and the
-- secrets of the installation's own scope, each change and each use (under the same hourly cap as a workspace's).
-- A workspace's are on that workspace's audit trail; these belong to no workspace, and no workspace sees them. It is
-- the vault's alone, as its other tables are (`ferrum.vault`), and append-only: nothing changes or removes a row. An
-- operator reads it with `ferrum task vault_events`. Every statement is safe to run again (see 20261119000201).
create table if not exists vault.event (
  id bigint generated always as identity primary key,
  at timestamptz not null default now(),
  -- What happened: `made`, `slot_added`, `installation_key_rotated`, a secret's `set`, `reveal`, `reveal_summary`, ...
  action text not null check (length(action) between 1 and 100),
  -- Whose secret it was about, and which (empty for the root's own events); never a value.
  owner text not null default '',
  name text not null default '',
  details jsonb not null default '{}',
  -- Who asked, when a person did; the request or job it was part of.
  actor uuid,
  request_id text not null default ''
);

-- The vault's own guard, so that nothing outside its schema is needed to keep the record as it was written.
create or replace function vault.forbid_change() returns trigger
language plpgsql as $$
begin
  raise exception 'ferrum: % on % is not allowed (append-only table)', tg_op, tg_table_name using errcode = 'P0001';
end $$;
drop trigger if exists event_append_only on vault.event;
create trigger event_append_only before update or delete on vault.event
  for each row execute function vault.forbid_change();
drop trigger if exists event_no_truncate on vault.event;
create trigger event_no_truncate before truncate on vault.event
  for each statement execute function vault.forbid_change();

alter table vault.event enable row level security;
alter table vault.event force row level security;
drop policy if exists vault_only on vault.event;
create policy vault_only on vault.event
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
