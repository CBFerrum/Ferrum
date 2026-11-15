-- Confirming it is you with a passkey of one's Ferrum account, at the accounts host, for a workspace's address that may
-- not use it (docs/operations.md, "Asking again before sensitive work"). Its request, the window's way back and the
-- one-use proof are a step-up's flow, as signing in again at a provider is; such a flow has no provider. An older build
-- makes none, and given one's state its provider's way back and SAML answer cannot read the row and refuse.
alter table sso_flow alter column provider_id drop not null;
