-- Automations answer a workflow transition within one document status (docs/builder/decisions.md BD-19).
alter table automation drop constraint automation_event_check;
alter table automation add constraint automation_event_check
  check (event in ('created', 'updated', 'submitted', 'cancelled', 'deleted', 'transitioned', 'scheduled'));
