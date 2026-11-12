-- The installed version of its package a backfill was last queued under (docs/builder/decisions.md BD-39): one whose
-- job failed is queued again once when its package changes version, the release that fixes it.
alter table backfill_run add column version text;
