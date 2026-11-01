-- Restriction of processing and retention rules (crates/kernel/src/retention.rs), beside the legal
-- holds and erasure that are already there: four separate operations, each with its own reason.
create table processing_restriction (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  email citext not null,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  lifted_by uuid,
  lifted_at timestamptz,
  primary key (tenant_id, id)
);
create unique index processing_restriction_live on processing_restriction (tenant_id, email) where lifted_at is null;

create table retention_rule (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  entity text not null,
  after_days int not null check (after_days between 1 and 36500),
  date_field text not null default 'updated_at',
  action text not null check (action in ('delete', 'anonymize')),
  condition jsonb not null default '{}',
  reason text not null check (length(reason) between 1 and 500),
  is_active boolean not null default true,
  last_run_at timestamptz,
  last_result jsonb,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

alter table processing_restriction enable row level security;
alter table processing_restriction force row level security;
create policy tenant_isolation on processing_restriction
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table retention_rule enable row level security;
alter table retention_rule force row level security;
create policy tenant_isolation on retention_rule
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
