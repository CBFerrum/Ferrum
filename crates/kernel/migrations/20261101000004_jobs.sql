-- Durable background work (crates/kernel/src/jobs.rs): what takes longer than a request or must
-- survive a restart. A worker holds a job by a lease it renews, and `lease_token` fences one that
-- lost its lease from writing an outcome over the next worker's.
create table job (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  kind text not null check (length(kind) between 1 and 100),
  payload jsonb not null default '{}',
  priority smallint not null default 1,
  status text not null default 'queued' check (status in ('queued', 'running', 'done', 'failed', 'cancelled')),
  attempts int not null default 0,
  max_attempts int not null default 5 check (max_attempts between 1 and 25),
  run_at timestamptz not null default now(),
  lease_until timestamptz,
  lease_token uuid,
  cancel_requested boolean not null default false,
  progress jsonb not null default '{}',
  result jsonb,
  last_error text,
  dedupe_key text check (length(dedupe_key) <= 200),
  created_by uuid,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  started_at timestamptz,
  finished_at timestamptz,
  primary key (tenant_id, id)
);
create index job_active on job (tenant_id, priority, run_at) where status in ('queued', 'running');
create index job_finished on job (finished_at) where finished_at is not null;
create unique index job_dedupe on job (tenant_id, kind, dedupe_key) where dedupe_key is not null and status in ('queued', 'running');

alter table job enable row level security;
alter table job force row level security;
create policy tenant_isolation on job
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
