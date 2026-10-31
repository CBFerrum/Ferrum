-- One person, many workspaces (docs/plan/apps-ui.md §9.3, §12.3; U6). In shapes B and C a person
-- signs in once, at the accounts host, as an identity: a person of the cluster's directory, the
-- reserved workspace `accounts`, so password, two-step sign-in, passkeys and browser sessions are
-- those any person has. A membership (`app_user` of a workspace) points at the identity it was
-- added to after the person proved it; the workspace's own password stays as it was, unused while
-- the cluster signs in through accounts.
alter table app_user add column identity_id uuid;
create unique index app_user_identity on app_user (tenant_id, identity_id) where identity_id is not null;
create index app_user_identity_of on app_user (identity_id) where identity_id is not null;
-- The memberships an address has across workspaces (signing in at accounts, adding a workspace).
create index app_user_email on app_user (email);
-- When a directory person's address was proven with a code sent to it.
alter table app_user add column email_verified_at timestamptz;

-- Codes sent to an address to prove it is the person's: to set a password at accounts, to finish
-- making an identity, to add a workspace. Only the newest works; five wrong tries spend it.
create table email_code (
  id uuid primary key default gen_random_uuid(),
  email citext not null check (length(email) <= 254),
  code_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  attempts int not null default 0,
  used_at timestamptz
);
create index email_code_email on email_code (email, created_at desc);
create index email_code_expiry on email_code (expires_at);
alter table email_code enable row level security;
alter table email_code force row level security;
create policy system_only on email_code using ((select ferrum_is_system())) with check ((select ferrum_is_system()));

-- A sign-in carried from the accounts host to a workspace's host: minted for one membership and
-- one host, taken once within a minute. `identity_id` is empty when the workspace's single sign-on
-- proved the person rather than an identity.
create table host_code (
  code_hash text primary key,
  tenant_id uuid not null,
  user_id uuid not null,
  identity_id uuid,
  host text not null check (length(host) <= 253),
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz,
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index host_code_expiry on host_code (expires_at);
alter table host_code enable row level security;
alter table host_code force row level security;
create policy system_only on host_code using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
