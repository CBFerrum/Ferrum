-- Receipts of business capabilities (crates/kernel/src/capability.rs, docs/logic): one row per logical
-- effect, committed in the same transaction as what it did. A repeat of the key finds it and gets what was
-- done; the same key with other input is refused. A key belongs to an effect, never to an attempt.
create table logic_effect (
  tenant_id uuid not null references tenant (id) on delete cascade,
  environment text not null check (length(environment) between 1 and 40),
  capability text not null check (length(capability) between 3 and 200),
  effect_key text not null check (length(effect_key) between 1 and 400),
  input_digest text not null,
  version text not null,
  capability_digest text not null,
  status text not null default 'succeeded' check (status in ('succeeded')),
  result jsonb not null default '{}',
  record_entity text,
  record_id uuid,
  actor uuid,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  primary key (tenant_id, environment, capability, effect_key)
);
create index logic_effect_created on logic_effect (created_at);

alter table logic_effect enable row level security;
alter table logic_effect force row level security;
create policy tenant_isolation on logic_effect
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
