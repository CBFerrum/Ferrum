-- Passkeys (WebAuthn; docs/plan/apps-ui.md §12.3, U6): credentials a person's devices keep, to sign
-- in without a password. The relying party is the host the passkey was made on (`rp_id`). A
-- credential belongs to one account in the cluster, so signing in finds the account by it.
create table passkey (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  credential_id bytea not null unique check (length(credential_id) between 16 and 1023),
  rp_id text not null check (length(rp_id) between 1 and 253),
  -- ES256: the uncompressed P-256 point; RS256: n's length (2 bytes), n, then e; EdDSA: 32 bytes.
  public_key bytea not null,
  alg int not null check (alg in (-7, -8, -257)),
  sign_count bigint not null default 0,
  name text not null default 'Passkey' check (length(name) between 1 and 100),
  transports text[] not null default '{}',
  created_at timestamptz not null default now(),
  last_used_at timestamptz,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index passkey_user on passkey (tenant_id, user_id);

alter table passkey enable row level security;
alter table passkey force row level security;
create policy tenant_isolation on passkey
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- A challenge is handed out once and taken once (five minutes): a signed answer cannot be replayed.
create table webauthn_challenge (
  id uuid primary key,
  challenge bytea not null,
  purpose text not null check (purpose in ('register', 'sign_in')),
  tenant_id uuid,
  user_id uuid,
  rp_id text not null,
  expires_at timestamptz not null
);
create index webauthn_challenge_expiry on webauthn_challenge (expires_at);

alter table webauthn_challenge enable row level security;
alter table webauthn_challenge force row level security;
create policy system_only on webauthn_challenge using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
