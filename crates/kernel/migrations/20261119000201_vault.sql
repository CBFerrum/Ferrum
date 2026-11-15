-- The vault (crates/kernel/src/vault, docs/operations.md "The vault"): the installation key wrapped by each of its
-- slots, every key below it wrapped by the one above, and the secrets sealed by their owner's key. No key that opens
-- anything is stored unwrapped here: a dump of these tables opens nothing without a slot's key, which is never stored.
--
-- Only the vault module reads or writes these tables: row-level security is forced, and the one policy asks for the
-- transaction-local setting `ferrum.vault = 'on'`, which nothing else sets (`ferrum.system` does not open them).
-- Every statement is safe to run again: a test that drops and makes the public schema again runs this file again, and
-- finds the vault as it left it (nothing here depends on the public schema).
create schema if not exists vault;

-- The installation key, wrapped once per slot: any slot opens it. One slot of each kind.
create table if not exists vault.root_slot (
  kind text primary key check (kind in ('file', 'kms', 'credential', 'recovery')),
  -- The installation key's version this slot wraps, and when that version was made: every slot wraps the current one.
  installation_version int not null check (installation_version > 0),
  installation_made_at timestamptz not null default now(),
  -- The installation key under this slot's key: nonce, ciphertext and tag (AES-256-GCM, bound to the kind and version).
  wrapped bytea not null,
  -- An HMAC of the slot's kind under the slot's key: tells a wrong key (a changed password) from a damaged row.
  check_value bytea not null,
  -- How the slot's key is made again: a salt (credential, recovery), Argon2id's costs (credential), how the
  -- key-management service is asked (kms: transit or command). Never a key.
  salt bytea,
  params jsonb not null default '{}',
  -- kms through Transit: the slot's key as the service's ciphertext.
  kms_wrapped text,
  created_at timestamptz not null default now()
);

-- Workspace keys (owner '') and owner keys, each version wrapped by the key above it.
create table if not exists vault.key (
  -- The workspace, or the nil id: the installation's own scope, for what belongs to no workspace.
  scope uuid not null,
  -- '' for the scope's workspace key; else the owner: platform, an app's id, or plugin:<name>.
  owner text not null check (owner = '' or owner ~ '^(platform|[a-z0-9][a-z0-9-]{0,62}|plugin:[a-z0-9][a-z0-9_.-]{0,99})$'),
  version int not null check (version > 0),
  -- The version of what wraps it: the installation key (a workspace key), or the scope's workspace key (an owner key).
  parent_version int not null check (parent_version > 0),
  -- An owner key's workspace key is the row of its scope with owner '': kept while an owner key names it.
  parent_owner text check ((owner = '' and parent_owner is null) or (owner <> '' and parent_owner = '')),
  wrapped bytea not null,
  -- Something sealed or derived with this owner key's version is kept outside the vault: it is not retired.
  outside boolean not null default false,
  created_at timestamptz not null default now(),
  primary key (scope, owner, version),
  foreign key (scope, parent_owner, parent_version) references vault.key (scope, owner, version)
);

-- Secrets, each version sealed by a version of its owner's key, which is kept while a secret names it.
create table if not exists vault.secret (
  scope uuid not null,
  owner text not null,
  name text not null check (name ~ '^[a-z0-9][a-z0-9_.-]{0,99}$'),
  version int not null check (version > 0),
  key_version int not null,
  kind text not null check (kind in ('value', 'ed25519')),
  sealed bytea not null,
  -- An Ed25519 key's public half, which is not secret.
  public_key bytea,
  -- What may be done with it (crate::vault::Rule): reveal, the hosts it may go to, a sandbox's treatment, the overlap
  -- of a rotation, the expiry.
  rule jsonb not null default '{}',
  -- A version a newer one replaced: it still verifies until then (its rule's overlap). Null: the current one.
  valid_until timestamptz,
  created_at timestamptz not null default now(),
  created_by uuid,
  -- The day it was last used, kept coarsely.
  last_used date,
  primary key (scope, owner, name, version),
  foreign key (scope, owner, key_version) references vault.key (scope, owner, version)
);
create index if not exists secret_by_key on vault.secret (scope, owner, key_version);

-- Uses of a secret in an hour, for the audit trail's cap (as sign-in failures are capped): the first ones are recorded
-- one by one, the rest counted and recorded as one summary once the hour is over.
create table if not exists vault.use_count (
  scope uuid not null,
  owner text not null,
  name text not null,
  verb text not null,
  hour timestamptz not null,
  n int not null,
  primary key (scope, owner, name, verb, hour)
);

alter table vault.root_slot enable row level security;
alter table vault.root_slot force row level security;
alter table vault.key enable row level security;
alter table vault.key force row level security;
alter table vault.secret enable row level security;
alter table vault.secret force row level security;
alter table vault.use_count enable row level security;
alter table vault.use_count force row level security;
drop policy if exists vault_only on vault.root_slot;
create policy vault_only on vault.root_slot
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
drop policy if exists vault_only on vault.key;
create policy vault_only on vault.key
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
drop policy if exists vault_only on vault.secret;
create policy vault_only on vault.secret
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
drop policy if exists vault_only on vault.use_count;
create policy vault_only on vault.use_count
  using (current_setting('ferrum.vault', true) = 'on') with check (current_setting('ferrum.vault', true) = 'on');
