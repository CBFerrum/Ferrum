-- Zones (docs/operations.md, "Zones"): the zone a host's requests are in, on replicas not pinned to one. Empty: the
-- caller's network or the default zone decides. Only `ferrum task add_host` and `set_host_zone` set it, never a
-- workspace's administrator.
alter table tenant_host add column zone text check (zone ~ '^[a-z0-9][a-z0-9-]{0,62}$');
