-- A workspace's keys made ready to move to another installation (crates/kernel/src/vault/scope.rs, `ferrum task
-- vault_transfer`): each version of its workspace key, wrapped by a transfer key made for this one move of this one
-- workspace. The transfer key is shown once to the operator and never stored: without it these rows open nothing.
-- A new transfer of the workspace replaces the one before; finish_move and the workspace's deletion remove them with
-- the rest of its vault. Every statement is safe to run again (see 20261119000201).
create table if not exists vault.transfer (
  scope uuid not null,
  version int not null check (version > 0),
  -- The transfer it belongs to: one id for all the versions it holds.
  id uuid not null,
  made_at timestamptz not null default now(),
  -- An HMAC under the transfer key: tells a wrong key from a damaged row.
  check_value bytea not null,
  -- The workspace key's version under the transfer key (AES-256-GCM, bound to the workspace, the transfer, the version).
  wrapped bytea not null,
  primary key (scope, version)
);

alter table vault.transfer enable row level security;
alter table vault.transfer force row level security;
drop policy if exists vault_only on vault.transfer;
create policy vault_only on vault.transfer
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
