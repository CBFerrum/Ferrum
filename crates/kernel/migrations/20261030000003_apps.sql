-- Apps. A release made available on the cluster contributes packages to the catalog (tables are
-- created for every workspace to use); workspaces install it separately. Bundles themselves live
-- in object storage; everything a replica needs to rebuild its catalog is here.
create table app_release (
  id uuid primary key default gen_random_uuid(),
  app_id text not null check (app_id ~ '^[a-z][a-z0-9-]{1,39}$'),
  version text not null,
  runtime text not null check (runtime in ('native', 'wasm', 'meta')),
  manifest jsonb not null,
  digest text not null,
  signatures jsonb,
  source text not null,
  bundle_key text,
  -- package name -> {relative path -> text}
  packages jsonb not null default '{}' check (jsonb_typeof(packages) = 'object'),
  plugin_sha256 text,
  status text not null default 'active' check (status in ('active', 'superseded', 'revoked', 'failed')),
  status_reason text,
  -- Workspace-private uploads (sandboxed code only, no tables) are visible to that workspace alone.
  private_tenant uuid references tenant(id) on delete cascade,
  created_by uuid,
  created_at timestamptz not null default now()
);
create unique index app_release_version on app_release (app_id, version) where private_tenant is null;
create unique index app_release_private_version on app_release (app_id, version, private_tenant) where private_tenant is not null;
create unique index app_release_active on app_release (app_id) where status = 'active' and private_tenant is null;
create unique index app_release_private_active on app_release (app_id, private_tenant) where status = 'active' and private_tenant is not null;

create table tenant_app (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app_id text not null,
  version text not null,
  enabled boolean not null default true,
  consent_digest text,
  plugin_id uuid,
  installed_by uuid,
  installed_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, app_id)
);

alter table tenant_app enable row level security;
alter table tenant_app force row level security;
create policy tenant_isolation on tenant_app using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
