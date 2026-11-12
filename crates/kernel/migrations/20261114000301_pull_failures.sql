-- A pull's failure streak (docs/builder/decisions.md BD-36): `told` says the connection's administrators heard of the
-- current streak of failed runs, so they hear once; a run that works ends the streak.
alter table conn_pull add column told boolean not null default false;
