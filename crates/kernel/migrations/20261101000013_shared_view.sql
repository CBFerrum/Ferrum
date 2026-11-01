-- A list view a team shares (crates/kernel/src/views.rs). A view is a question, not an answer: what
-- each person gets from it is still what they may read.
create table shared_view (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  entity text not null,
  name text not null check (length(name) between 1 and 80),
  value jsonb not null default '{}',
  created_by uuid not null,
  version int not null default 1,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create unique index shared_view_name on shared_view (tenant_id, entity, lower(name));

alter table shared_view enable row level security;
alter table shared_view force row level security;
create policy tenant_isolation on shared_view using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
