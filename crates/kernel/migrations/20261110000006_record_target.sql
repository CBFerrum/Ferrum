-- Service targets (docs/api.md, "Service targets"; docs/builder/decisions.md BD-04): one row per record and
-- target, kept by the record's saves. `round` counts the rounds begun; a round not started has no
-- `started_at`. At a pause `worked_secs` grows by the open time since `resumed_at`; at a resume `resumed_at`
-- is now and `due_at` the remaining time from now. `breached_at` is set once per round, by an update that
-- only succeeds while it is empty.
create table record_target (
  tenant_id uuid not null references tenant (id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  target text not null,
  round int not null default 0,
  started_at timestamptz,
  resumed_at timestamptz,
  paused_at timestamptz,
  worked_secs bigint not null default 0,
  within_secs bigint not null default 0,
  due_at timestamptz,
  met_at timestamptz,
  breached_at timestamptz,
  primary key (tenant_id, entity, record_id, target)
);
-- What the minute loop sweeps: running rounds past their due moment, not yet breached.
create index record_target_due on record_target (due_at)
  where started_at is not null and met_at is null and paused_at is null and breached_at is null;

alter table record_target enable row level security;
alter table record_target force row level security;
create policy tenant_isolation on record_target using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
