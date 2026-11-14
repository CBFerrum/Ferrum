-- SAML: encrypted assertions and single logout (crates/kernel/src/sso.rs and saml_logout.rs, crates/saml).

-- The keys a workspace's SAML assertions may be encrypted to, apart from the key its requests are signed with. Made
-- the first time wanted (by whichever replica gets there first: one current key at most), the private half sealed by
-- the server key. A replaced key keeps being published and accepted until `retires_at`.
create table saml_enc_key (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  private_key text not null,
  certificate text not null,
  created_at timestamptz not null default now(),
  retires_at timestamptz,
  primary key (tenant_id, id)
);
create unique index saml_enc_key_current on saml_enc_key (tenant_id) where retires_at is null;
alter table saml_enc_key enable row level security;
alter table saml_enc_key force row level security;
create policy system_only on saml_enc_key using ((select ferrum_is_system())) with check ((select ferrum_is_system()));

-- A SAML provider's own addresses and single logout. `saml_sp_base`: the origin its entity ID and addresses are
-- under, kept from when it was saved, so they never change under it; null on providers saved before, which keep
-- PUBLIC_URL's as then until saved again. `slo_out` (logout requests sent from here) and `slo_in` (taken from the
-- provider): null on providers saved before, which have none, as then; saving sets them.
alter table sso_provider
  add column saml_sp_base text,
  add column slo_out boolean,
  add column slo_in boolean;

-- What a SAML sign-in's logout needs, kept on its session and going with it: `{provider, name_id, format,
-- session_index}`.
alter table user_session add column saml jsonb;
create index user_session_saml on user_session (tenant_id, (saml->>'provider'), (saml->>'name_id')) where saml is not null and revoked_at is null;

-- A logout this server starts, for ten minutes: the ticket the browser carries to send the request, then the
-- request's ID until the provider answers, and where the browser lands after.
create table saml_logout (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  ticket_hash text unique,
  request_id text unique,
  provider_id uuid not null,
  saml jsonb not null,
  return_to text not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
alter table saml_logout enable row level security;
alter table saml_logout force row level security;
create policy system_only on saml_logout using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
