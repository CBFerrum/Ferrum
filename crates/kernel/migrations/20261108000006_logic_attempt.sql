-- Each step's attempts, so an operator can tell how often a step was tried and how its last try went
-- (FLC-OPS-001): the receipt says what succeeded, this says what came before it.
create table logic_attempt (
  tenant_id uuid not null references tenant (id) on delete cascade,
  run_id uuid not null,
  activation text not null check (length(activation) between 1 and 1000),
  attempts int not null default 0,
  last_outcome text not null default '' check (length(last_outcome) <= 500),
  first_at timestamptz not null default now(),
  last_at timestamptz not null default now(),
  primary key (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run (tenant_id, id) on delete cascade
);
alter table logic_attempt enable row level security;
alter table logic_attempt force row level security;
create policy tenant_isolation on logic_attempt
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
