-- Print formats (crates/kernel/src/documents.rs and layout.rs, docs/builder/decisions.md BD-28).

-- The workspace's chosen default document per entity: a choice of its own, so that making a package's format the
-- default does not make an edited copy of it.
create table document_default (
  tenant_id uuid not null references tenant (id) on delete cascade,
  entity text not null,
  key text not null check (length(key) between 1 and 100),
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, entity)
);

-- Every saved version of a workspace's template, the last 50 per key kept; a restore is a save of an old one.
create table document_template_version (
  tenant_id uuid not null references tenant (id) on delete cascade,
  key text not null,
  n int not null,
  def jsonb not null,
  saved_by uuid,
  saved_at timestamptz not null default now(),
  primary key (tenant_id, key, n)
);

alter table document_default enable row level security;
alter table document_default force row level security;
create policy tenant_isolation on document_default using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table document_template_version enable row level security;
alter table document_template_version force row level security;
create policy tenant_isolation on document_template_version using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table document_template add column created_by uuid, add column created_at timestamptz not null default now();

