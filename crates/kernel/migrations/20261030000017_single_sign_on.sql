-- Single sign-on through OpenID Connect providers (Microsoft Entra ID, Okta, Google Workspace,
-- Keycloak, Auth0…), set up per workspace with the client secret sealed by the server key; the
-- sign-ins on their way through a provider (state, nonce and PKCE verifier, ten minutes); and each
-- person's link to their identity at the provider, so a reused email address cannot take over an
-- account.
create table sso_provider (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  label text not null check (length(label) between 1 and 60),
  issuer text not null,
  client_id text not null,
  client_secret text,
  domains text[] not null default '{}',
  auto_create boolean not null default false,
  default_roles text[] not null default '{}',
  enabled boolean not null default true,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
alter table sso_provider enable row level security;
alter table sso_provider force row level security;
create policy tenant_isolation on sso_provider
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

create table sso_flow (
  state_hash text primary key,
  tenant_id uuid not null,
  provider_id uuid not null,
  nonce text not null,
  verifier text not null,
  redirect_uri text not null,
  binding_hash text not null,
  created_at timestamptz not null default now(),
  foreign key (tenant_id, provider_id) references sso_provider (tenant_id, id) on delete cascade
);
create index sso_flow_created on sso_flow (created_at);
-- Sign-ins run before anyone is signed in: only the system context touches this table.
alter table sso_flow enable row level security;
alter table sso_flow force row level security;
create policy system_only on sso_flow using ((select ferrum_is_system())) with check ((select ferrum_is_system()));

alter table app_user add column sso_subject text;
create unique index app_user_sso_subject on app_user (tenant_id, sso_subject) where sso_subject is not null;
