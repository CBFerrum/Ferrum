-- Durable processes' records (crates/kernel/src/process.rs, docs/logic/processes.md). The durable runtime keeps
-- the schedule; Ferrum keeps the business record: which runs were accepted on which release, what people were
-- asked and what they decided, what the runs wait for, the signals that came for them, and its calendars.

-- A run, accepted once per business key: its row is its start intent. The release is the one active when the
-- trigger was accepted, never "the latest" later.
create table logic_run (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  release_id uuid not null,
  solution text not null,
  process text not null,
  business_key text not null check (length(business_key) between 1 and 400),
  trigger jsonb not null,
  trigger_event bigint,
  requester uuid,
  status text not null default 'starting'
    check (status in ('starting', 'running', 'needs_attention', 'cancelling', 'completed', 'refused', 'cancelled')),
  -- The state machine's run as the runtime last reported it: a read model, rebuilt from the runtime, never
  -- what schedules anything.
  state jsonb,
  reported int not null default 0,
  workflow_id text not null,
  output jsonb,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  accepted_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  finished_at timestamptz,
  primary key (tenant_id, id),
  unique (tenant_id, solution, process, business_key),
  foreign key (tenant_id, release_id) references logic_release (tenant_id, id)
);
create index logic_run_open on logic_run (tenant_id, status) where finished_at is null;

-- What a run asks of people: a task to decide, a case and its tasks, a recovery, something to look at.
create table logic_task (
  tenant_id uuid not null,
  id uuid not null default gen_random_uuid(),
  run_id uuid not null,
  activation text not null check (length(activation) between 1 and 1000),
  kind text not null check (kind in ('task', 'case', 'recovery', 'attention')),
  label text not null,
  role text not null default '',
  material jsonb not null default '{}',
  digest text not null default '',
  form jsonb not null default '[]',
  approvals int not null default 1 check (approvals between 1 and 9),
  delegation int not null default 0 check (delegation between 0 and 3),
  -- Who may not decide it, whatever their roles: the requester.
  exclude uuid[] not null default '{}',
  -- Who it was handed to, in order; the first is whoever handed it on first.
  delegated uuid[] not null default '{}',
  -- A case's tasks, or what a recovery may do.
  detail jsonb not null default '{}',
  status text not null default 'open'
    check (status in ('open', 'approved', 'rejected', 'timed_out', 'cancelled', 'done', 'resolved')),
  deadline timestamptz,
  -- What the deadline was worked out from: calendar, revision, zone, zone data, adjustment.
  deadline_basis jsonb,
  version int not null default 1,
  opened_at timestamptz not null default now(),
  closed_at timestamptz,
  primary key (tenant_id, id),
  unique (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run (tenant_id, id) on delete cascade
);
create index logic_task_open on logic_task (tenant_id, status, role) where status = 'open';

-- Decisions never change: who decided what, on which material, when, with what they filled in.
create table logic_task_decision (
  tenant_id uuid not null,
  id uuid not null default gen_random_uuid(),
  task_id uuid not null,
  by_user uuid not null,
  decision text not null check (decision in ('approve', 'reject', 'delegate', 'done', 'close', 'resume', 'end', 'acknowledge')),
  -- For a case: which of its tasks.
  part text not null default '',
  to_user uuid,
  digest text not null default '',
  values jsonb not null default '{}',
  reason text not null default '' check (length(reason) <= 2000),
  decided_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, task_id) references logic_task (tenant_id, id) on delete cascade
);
create unique index logic_task_decision_once on logic_task_decision (tenant_id, task_id, by_user, part) where decision in ('approve', 'reject', 'done');

create function logic_task_decision_immutable() returns trigger language plpgsql as $$
begin
  raise exception 'a decision does not change';
end $$;
create trigger logic_task_decision_immutable before update on logic_task_decision for each row execute function logic_task_decision_immutable();

-- What a run waits for, and the signals that came, before or after it began to wait.
create table logic_wait (
  tenant_id uuid not null,
  run_id uuid not null,
  activation text not null,
  event text not null,
  correlation text not null,
  status text not null default 'waiting' check (status in ('waiting', 'matched', 'timed_out', 'cancelled')),
  deadline timestamptz,
  signal_id bigint,
  created_at timestamptz not null default now(),
  primary key (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run (tenant_id, id) on delete cascade
);
create index logic_wait_match on logic_wait (tenant_id, event, correlation) where status = 'waiting';

create table logic_signal (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id bigint generated always as identity,
  event_id bigint not null,
  event text not null,
  correlation text not null,
  payload jsonb not null,
  payload_digest text not null,
  emitter uuid,
  status text not null default 'pending' check (status in ('pending', 'delivered', 'expired', 'quarantined')),
  run_id uuid,
  activation text,
  reason text,
  arrived_at timestamptz not null default now(),
  expires_at timestamptz not null,
  primary key (tenant_id, id)
);
-- An event kept for a wait to come is kept once; one handed to a waiting run, once per run and wait.
create unique index logic_signal_early on logic_signal (tenant_id, event_id, event) where run_id is null;
create unique index logic_signal_matched on logic_signal (tenant_id, event_id, event, run_id, activation) where run_id is not null;
create index logic_signal_pending on logic_signal (tenant_id, event, correlation) where status = 'pending';

-- A workspace's business calendars. A new revision changes the deadlines resolved after it, never those before.
create table logic_calendar (
  tenant_id uuid not null references tenant (id) on delete cascade,
  name text not null check (name ~ '^[a-z][a-z0-9_]{0,63}$'),
  zone text not null,
  weekends text[] not null default '{sat,sun}',
  holidays date[] not null default '{}',
  revision bigint not null default 1,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, name)
);

alter table logic_run enable row level security;
alter table logic_run force row level security;
create policy tenant_isolation on logic_run
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_task enable row level security;
alter table logic_task force row level security;
create policy tenant_isolation on logic_task
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_task_decision enable row level security;
alter table logic_task_decision force row level security;
create policy tenant_isolation on logic_task_decision
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_wait enable row level security;
alter table logic_wait force row level security;
create policy tenant_isolation on logic_wait
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_signal enable row level security;
alter table logic_signal force row level security;
create policy tenant_isolation on logic_signal
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_calendar enable row level security;
alter table logic_calendar force row level security;
create policy tenant_isolation on logic_calendar
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
