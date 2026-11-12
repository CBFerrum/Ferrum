-- What a round was last worked out from (docs/builder/decisions.md BD-41): the calendar it counts on ('' plain
-- time) and the record's start moment it was counted from. A change of either on an open round works its due moment
-- out again once. `anchored` false: a round from before this release, whose anchors are recorded at its next save
-- without working anything out again.
alter table record_target add column calendar text;
alter table record_target add column start_from timestamptz;
alter table record_target add column anchored boolean not null default false;
