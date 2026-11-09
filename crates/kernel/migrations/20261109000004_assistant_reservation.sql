-- Tokens held back from a workspace's monthly allowance for a call to the model in flight
-- (server/src/assistant.rs, FEO-062): taken before the call, under one lock per workspace, and let go
-- when the call's use is recorded or it fails. One left by a server that stopped counts for 15 minutes.
create table assistant_reservation (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  user_id uuid not null,
  tokens bigint not null check (tokens > 0),
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

alter table assistant_reservation enable row level security;
alter table assistant_reservation force row level security;
create policy tenant_isolation on assistant_reservation
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
