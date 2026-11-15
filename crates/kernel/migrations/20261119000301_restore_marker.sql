-- A restore noticed on any service (docs/plan/stateless.md, D7; docs/operations.md, "After restoring a backup"): a
-- counter that only moves forward, kept here and in object storage beside the journals, written here first. A database
-- that comes up behind the store's counter has been taken back in time, whatever its identity says.
-- NULL: a database from before the marker. 0: one born with it, which nothing can be behind yet.
alter table platform_incarnation add column if not exists restore_marker bigint;
-- When this database last moved it on (one replica moves it at a time, at most every 30 seconds).
alter table platform_incarnation add column if not exists restore_marker_at timestamptz;
