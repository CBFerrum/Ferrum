-- The kernel's own stores of sealed values, behind the vault (docs/plan/stateless.md, D5 second half): secret fields,
-- connections' credentials and tokens, message channels' secrets, single sign-on secrets, SAML keys, second factors,
-- webhook secrets, approvals' secret parameters and the push key are each a secret of their owner in the vault. Their
-- old columns stay as the previous build reads them, and nothing is dropped.
--
-- For each version of such a secret, a digest of what its old store held when it was written: where the old store holds
-- something else, an older build changed it since, and the old store is read until the move takes it again. A table of
-- its own, so that what reads the vault's secrets before this migration is applied (a server opening the vault as it
-- starts, ahead of migrating) reads them as before. It goes with its version. Every statement is safe to run again (a
-- test's reset of the public schema runs it again on the vault it left).
create table if not exists vault.kept (
  scope uuid not null,
  owner text not null,
  name text not null,
  version int not null,
  of text not null check (of ~ '^[0-9a-f]{64}$'),
  primary key (scope, owner, name, version),
  foreign key (scope, owner, name, version) references vault.secret (scope, owner, name, version) on delete cascade
);
alter table vault.kept enable row level security;
alter table vault.kept force row level security;
drop policy if exists vault_only on vault.kept;
create policy vault_only on vault.kept
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');

-- Where the old master key (FERRUM_SECRET_KEY) and the readers that pass the vault by were last used, by what used them
-- (`open:webhook`, `reader:Kernel::secret`, ...) and, for a reader, the app code that called it: what `ferrum task
-- legacy_key` and the doctor say still depends on the old key. Never a value. `moved` (no caller): the move found
-- nothing waiting, since `first_at`.
create table if not exists legacy_key_use (
  what text not null check (length(what) between 1 and 100),
  caller text not null default '' check (length(caller) <= 300),
  first_at timestamptz not null default now(),
  last_at timestamptz not null default now(),
  primary key (what, caller)
);
alter table legacy_key_use enable row level security;
alter table legacy_key_use force row level security;
drop policy if exists system_only on legacy_key_use;
create policy system_only on legacy_key_use using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
