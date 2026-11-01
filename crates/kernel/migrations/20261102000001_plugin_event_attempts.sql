-- A plugin's run for an event is recorded when it starts and again when it ends. A row that never
-- finished means the run may have done its work: the retry is told so, and how many came before it.
alter table plugin_event_run add column attempts int not null default 1 check (attempts >= 1);
alter table plugin_event_run add column finished_at timestamptz;
update plugin_event_run set finished_at = ran_at;
