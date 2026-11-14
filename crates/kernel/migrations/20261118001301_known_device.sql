-- A sign-in from a new device (docs/operations.md, "Staff networks"): the devices each person has signed in
-- from, by the browser's user agent with its version numbers taken out. Kept with the account, gone with it.
create table known_device (
  tenant_id uuid not null,
  user_id uuid not null,
  device text not null,
  first_seen timestamptz not null default now(),
  primary key (tenant_id, user_id, device),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

alter table known_device enable row level security;
alter table known_device force row level security;
create policy tenant_isolation on known_device
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
