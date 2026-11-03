-- SAML 2.0 identity providers beside OpenID Connect ones (crates/kernel/src/sso.rs, crates/saml). A
-- SAML provider keeps its metadata (the XML, or the https address it is fetched from at each sign-in)
-- in place of a client; its entity ID stands in the issuer column. A SAML sign-in comes back as a
-- response the provider posts to the server, which checks it and keeps who it names on the flow,
-- under a one-time code the browser then finishes with, as with OpenID Connect.
alter table sso_provider
  add column protocol text not null default 'oidc' check (protocol in ('oidc', 'saml')),
  add column saml_metadata text;
alter table sso_provider add constraint sso_provider_saml_metadata check ((protocol = 'saml') = (saml_metadata is not null));

alter table sso_flow
  add column code_hash text,
  add column answer jsonb;

-- Each workspace's own SAML key: it signs the workspace's requests and opens assertions encrypted to
-- it; its certificate is in the workspace's metadata. Made the first time it is wanted, the private
-- half sealed by the server key.
create table saml_sp_key (
  tenant_id uuid primary key references tenant (id) on delete cascade,
  private_key text not null,
  certificate text not null,
  created_at timestamptz not null default now()
);
alter table saml_sp_key enable row level security;
alter table saml_sp_key force row level security;
create policy system_only on saml_sp_key using ((select ferrum_is_system())) with check ((select ferrum_is_system()));
