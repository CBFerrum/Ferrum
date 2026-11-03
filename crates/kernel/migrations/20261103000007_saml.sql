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
