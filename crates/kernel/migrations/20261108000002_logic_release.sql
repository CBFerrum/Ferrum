-- Customer logic's releases, reviews and activations (crates/kernel/src/logic.rs, docs/logic/rules.md).
--
-- A release is what a solution's source compiled to, kept with the source: it never changes. A review is
-- of a release's artifact digest, by someone other than its author. An activation says which release of a
-- solution runs in a workspace, with a generation compared on every change. A refusal is evidence that a
-- rule refused a save, kept when the rule asks for it, written beside the save's rolled-back transaction.
create table logic_release (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  solution text not null check (length(solution) between 2 and 101),
  version text not null check (length(version) between 5 and 32),
  source_digest text not null,
  artifact_digest text not null,
  compiler text not null,
  profile text not null check (profile in ('core', 'full_process')),
  source jsonb not null,
  release jsonb not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, solution, version)
);

create function logic_release_immutable() returns trigger language plpgsql as $$
begin
  raise exception 'a release does not change: compile a new version';
end $$;
create trigger logic_release_immutable before update on logic_release for each row execute function logic_release_immutable();

create table logic_review (
  tenant_id uuid not null,
  release_id uuid not null,
  -- What the reviewer was shown and decided on.
  artifact_digest text not null,
  decided_by uuid not null,
  decision text not null check (decision in ('approved', 'rejected')),
  reason text not null default '' check (length(reason) <= 2000),
  decided_at timestamptz not null default now(),
  primary key (tenant_id, release_id, decided_by),
  foreign key (tenant_id, release_id) references logic_release (tenant_id, id) on delete cascade
);

create table logic_activation (
  tenant_id uuid not null,
  solution text not null,
  release_id uuid not null,
  generation bigint not null default 1,
  activated_by uuid not null,
  activated_at timestamptz not null default now(),
  primary key (tenant_id, solution),
  foreign key (tenant_id, release_id) references logic_release (tenant_id, id)
);

create table logic_refusal (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id bigint generated always as identity,
  release_id uuid not null,
  rule text not null,
  entity text not null,
  record_id uuid,
  actor uuid,
  -- 'false': the check did not hold; 'unknown': it could not be decided, which refuses too.
  outcome text not null check (outcome in ('false', 'unknown')),
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index logic_refusal_created on logic_refusal (tenant_id, created_at);

alter table logic_release enable row level security;
alter table logic_release force row level security;
create policy tenant_isolation on logic_release
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_review enable row level security;
alter table logic_review force row level security;
create policy tenant_isolation on logic_review
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_activation enable row level security;
alter table logic_activation force row level security;
create policy tenant_isolation on logic_activation
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table logic_refusal enable row level security;
alter table logic_refusal force row level security;
create policy tenant_isolation on logic_refusal
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- How many events lie between an event and the change that started its chain: work done while handling an
-- event writes events one further (db.rs, Trace). A chain too long, or one change followed by too many events,
-- is stopped and kept as a dead event saying why (FLC-HOOK-008).
alter table outbox add column depth smallint not null default coalesce(nullif(current_setting('ferrum.depth', true), '')::smallint, 0);

alter table job add column depth smallint not null default coalesce(nullif(current_setting('ferrum.depth', true), '')::smallint, 0);

create table event_chain (
  tenant_id uuid not null references tenant (id) on delete cascade,
  correlation_id text not null,
  events int not null default 0,
  first_at timestamptz not null default now(),
  primary key (tenant_id, correlation_id)
);
alter table event_chain enable row level security;
alter table event_chain force row level security;
create policy tenant_isolation on event_chain
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
