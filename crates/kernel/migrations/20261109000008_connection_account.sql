-- Connections that use one provider account (the same provider and credentials) are known by a keyed
-- fingerprint of those credentials, which reveals nothing of them: when the provider tells one of them to
-- wait, every connection on that account waits with it (crates/kernel/src/connect.rs, FEO-067).
alter table connection add column account_key text;
create index connection_account on connection (account_key) where account_key is not null;
