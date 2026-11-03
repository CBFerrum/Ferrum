-- Devices that call in, like chargers, reach whichever gateway a load balancer gives them. A domain can
-- be pooled across a site's gateways, and each device then belongs to the gateway holding its connection.
alter table op_authority add column pooled boolean not null default false;

-- One row per device, and the generation only rises, so what an old connection says about itself cannot
-- be mistaken for what is true now.
create table op_connection (
  tenant_id uuid not null,
  device_id uuid not null,
  gateway_id uuid not null,
  generation bigint not null default 1,
  opened_at timestamptz not null,
  closed_at timestamptz,
  primary key (tenant_id, device_id)
);
create index op_connection_gateway on op_connection (tenant_id, gateway_id) where closed_at is null;
alter table op_connection enable row level security;
alter table op_connection force row level security;
create policy tenant_isolation on op_connection using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- Which gateway was given a command, and over which connection. Only that gateway's word counts.
alter table op_command add column taken_by uuid;
alter table op_command add column connection_generation bigint;
