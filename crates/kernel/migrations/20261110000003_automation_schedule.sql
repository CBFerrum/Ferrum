-- Automations that fire on time (docs/api.md, Automations): an automation may carry a schedule in place
-- of a record event, and its event is then `scheduled`. A loop in every replica looks at the window from
-- `scanned_until` to now; a large entity is worked through a page at a time, `scan_to` holding the end of
-- the window being worked through and `scan_cursor` the last record done. Each (record, due moment) fires
-- once: `automation_occurrence`'s primary key says so, however many replicas look.
alter table automation add column schedule jsonb;
alter table automation add column scanned_until timestamptz;
alter table automation add column scan_to timestamptz;
alter table automation add column scan_cursor uuid;
alter table automation drop constraint automation_event_check;
alter table automation add constraint automation_event_check
  check (event in ('created', 'updated', 'submitted', 'cancelled', 'deleted', 'scheduled'));
alter table automation add constraint automation_schedule_check check ((event = 'scheduled') = (schedule is not null));
create index automation_scheduled on automation (tenant_id, scanned_until) where event = 'scheduled' and is_active;

create table automation_occurrence (
  tenant_id uuid not null,
  automation_id uuid not null,
  record_id uuid not null,
  due_at timestamptz not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, automation_id, record_id, due_at),
  foreign key (tenant_id, automation_id) references automation (tenant_id, id) on delete cascade
);
create index automation_occurrence_due on automation_occurrence (due_at);

alter table automation_occurrence enable row level security;
alter table automation_occurrence force row level security;
create policy tenant_isolation on automation_occurrence using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
