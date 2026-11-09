-- Who stands in for someone while they are away (crates/kernel/src/process.rs, FEO-034): a task handed to
-- them may be decided by their substitute between these days, as they could have, never on the
-- substitute's own request.
create table person_away (
  tenant_id uuid not null references tenant (id) on delete cascade,
  user_id uuid not null,
  starts_on date not null,
  ends_on date not null check (ends_on >= starts_on),
  substitute uuid not null check (substitute <> user_id),
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, user_id, starts_on)
);
create index person_away_substitute on person_away (tenant_id, substitute, ends_on);

alter table person_away enable row level security;
alter table person_away force row level security;
create policy tenant_isolation on person_away
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
