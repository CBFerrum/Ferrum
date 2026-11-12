-- Approvals on a workflow transition (crates/kernel/src/approvals.rs, docs/builder/decisions.md BD-19). Taking a
-- transition whose policy has steps to decide makes a request; at most one is pending per record. `policy` is the
-- policy as it was when asked (a change to the workflow does not move a request under way), `steps` each step's
-- state, approvers and due moments, `next_due` the first reminder or escalation the minute loop owes.
create table approval_request (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  action text not null,
  from_state text not null,
  requester uuid,
  params jsonb not null default '{}',
  policy jsonb not null,
  steps jsonb not null,
  status text not null default 'pending' check (status in ('pending', 'approved', 'rejected', 'recalled', 'cancelled')),
  next_due timestamptz,
  created_at timestamptz not null default now(),
  ended_at timestamptz,
  primary key (tenant_id, id)
);
create unique index approval_request_pending on approval_request (tenant_id, entity, record_id) where status = 'pending';
create index approval_request_due on approval_request (next_due) where status = 'pending' and next_due is not null;
create index approval_request_record on approval_request (tenant_id, entity, record_id, created_at);

-- Each decision, kept: who decided, for whom when standing in, under which step, and what. `void` when a change to
-- a field the policy names cancelled the decisions so far; nothing else changes a decision.
create table approval_decision (
  tenant_id uuid not null,
  id uuid not null,
  request_id uuid not null,
  step int not null check (step >= 0),
  decider uuid not null,
  on_behalf_of uuid,
  outcome text not null check (outcome in ('approve', 'reject')),
  comment text check (length(comment) <= 2000),
  at timestamptz not null default now(),
  void boolean not null default false,
  primary key (tenant_id, id),
  foreign key (tenant_id, request_id) references approval_request (tenant_id, id) on delete cascade
);
create index approval_decision_request on approval_decision (tenant_id, request_id);

alter table approval_request enable row level security;
alter table approval_request force row level security;
create policy tenant_isolation on approval_request
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table approval_decision enable row level security;
alter table approval_decision force row level security;
create policy tenant_isolation on approval_decision
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
