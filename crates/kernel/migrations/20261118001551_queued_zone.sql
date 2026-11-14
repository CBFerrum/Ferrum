-- Zones, level 2 (docs/operations.md, "Zones"): the zone of the request that queued a job or an event, started a run
-- or asked for an approval, when that zone narrows staff. Work done for it later is done in that zone, by name, against
-- the worker's own zone settings; a name they do not hold fails that work. Empty for work queued from inside (no
-- request), as for everything queued before this, which runs as before.
alter table job add column zone text default nullif(current_setting('ferrum.zone', true), '');
alter table outbox add column zone text default nullif(current_setting('ferrum.zone', true), '');
alter table logic_run add column zone text default nullif(current_setting('ferrum.zone', true), '');
alter table approval_request add column zone text default nullif(current_setting('ferrum.zone', true), '');
