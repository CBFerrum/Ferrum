-- A domain of a workspace's own proven by its owner (docs/operations.md, "Custom domains"): when it became verified, its
-- DNS held `_ferrum.{host}` TXT `ferrum-domain={verify_token}`, a token no other workspace's row ever has. Empty: never
-- proven by a record. Domains verified before this column count as proven, so none of them is ever asked for a record;
-- one once proven is verified again by pointing here alone. An older build reads none of it.
alter table tenant_host add column proved_at timestamptz;
update tenant_host set proved_at = verified_at where kind = 'custom' and verified_at is not null;

-- The addresses of a workspace that left this database (deleted, or moved away and finished), kept from every other
-- workspace until `until`, as a released address is (`HOST_QUARANTINE_DAYS`): the workspace's rows go, so the names are
-- kept here. No workspace column, so nothing that deletes, copies or moves a workspace's rows reaches it.
create table host_quarantine (
  host text primary key,
  until timestamptz not null
);
