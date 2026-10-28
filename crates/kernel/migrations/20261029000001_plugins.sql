-- Tenant plugins: sandboxed WebAssembly modules bound to document hooks.
create table plugin (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null check (name ~ '^[a-z][a-z0-9_-]{1,62}$'),
  version text not null default '0.1.0',
  description text,
  sha256 text not null,
  wasm bytea not null check (octet_length(wasm) <= 8388608),
  -- [{"entity": "SalesInvoice", "stage": "validate"}, ...]
  hooks jsonb not null default '[]' check (jsonb_typeof(hooks) = 'array'),
  fuel_limit bigint not null default 50000000 check (fuel_limit between 10000 and 2000000000),
  memory_limit_mb int not null default 32 check (memory_limit_mb between 1 and 256),
  enabled boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, name)
);

alter table plugin enable row level security;
alter table plugin force row level security;
create policy tenant_isolation on plugin
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
