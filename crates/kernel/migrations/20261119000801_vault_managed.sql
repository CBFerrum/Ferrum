-- A secret its app manages itself (crates/kernel/src/vault/secrets.rs, `Vault::manage`): the routes people use to set,
-- rotate, remove, change the rule of or regenerate a secret refuse it, naming the app. A row of its own, so that the
-- previous build, which reads every secret's rule strictly, never meets it; carried with the workspace's vault.
create table if not exists vault.managed (
  scope uuid not null,
  owner text not null,
  name text not null,
  managed_at timestamptz not null default now(),
  primary key (scope, owner, name)
);
alter table vault.managed enable row level security;
alter table vault.managed force row level security;
drop policy if exists vault_only on vault.managed;
create policy vault_only on vault.managed
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
