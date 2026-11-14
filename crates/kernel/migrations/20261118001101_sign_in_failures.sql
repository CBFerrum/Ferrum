-- Failed sign-ins of a known account, counted by the hour (docs/operations.md, "Staff networks"): the
-- audit trail records the first few of an hour one by one, then one summary with how many more there were.
create table sign_in_failure (
  tenant_id uuid not null,
  user_id uuid not null,
  hour timestamptz not null,
  n int not null default 1,
  primary key (tenant_id, user_id, hour),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

create index sign_in_failure_hour on sign_in_failure (hour);

alter table sign_in_failure enable row level security;
alter table sign_in_failure force row level security;
create policy tenant_isolation on sign_in_failure
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
