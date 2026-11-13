-- Hosts the operator vouches for (`ferrum task add_host`, docs/operations.md, "Zones"): names that only resolve inside a
-- private network, which the server cannot verify over the internet. Served as verified (their `verified_at` is set
-- when they are added), never checked by the verifier, which looks at `custom` hosts alone.
alter table tenant_host drop constraint tenant_host_kind_check; -- window: safe, re-added at once below, wider
-- window: safe, wider than before: a build before this one serves an operator row as verified (it reads only
-- `kind = 'subdomain' or verified_at is not null`), and checks, removes and releases only `custom` rows.
alter table tenant_host add constraint tenant_host_kind_check check (kind in ('subdomain', 'custom', 'operator'));
