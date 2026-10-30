-- One-time steps that move an existing workspace onto a new release's layout (docs/plan/apps-ui.md
-- §13): which ran, and the packages each installed, so that going back to the release before can
-- take them out again (`ferrum task upgrade_back step:<name>`).
create table upgrade_step (
  tenant_id uuid not null references tenant (id) on delete cascade,
  step text not null check (length(step) <= 60),
  installed text[] not null default '{}',
  done_at timestamptz not null default now(),
  primary key (tenant_id, step)
);
alter table upgrade_step enable row level security;
alter table upgrade_step force row level security;
create policy tenant_isolation on upgrade_step
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
