-- One writer per event (FLC-MIG-003). Each run records the generation of its solution's activation it was
-- accepted under, and an automation's events, once the solution imported from it runs in its place, are claimed
-- in the automation's own record of runs as handed over, by whichever of the two comes to the event first.
alter table logic_run add column generation bigint;

alter table automation_run drop constraint automation_run_status_check;
alter table automation_run add constraint automation_run_status_check check (status in ('done', 'skipped', 'failed', 'handed_over'));
