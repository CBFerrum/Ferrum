-- Zones (docs/operations.md, "Zones"): the zone a browser session was opened in, shown where sessions are listed.
-- Empty: opened without zones, or before them.
alter table user_session add column zone text;
