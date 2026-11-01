-- A provider that said to wait, or keeps failing, is left alone for a while: every job of the
-- connection waits together. `failures` counts failures in a row.
alter table connection
  add column failures int not null default 0,
  add column waiting_until timestamptz,
  add column waiting_reason text;
