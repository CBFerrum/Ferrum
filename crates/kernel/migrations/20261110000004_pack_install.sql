-- Packs installed in a workspace (docs/api.md, "Packs"; docs/builder/decisions.md BD-07): which pack, its
-- version and the digest of what was installed, who installed it and when, and what the install did to
-- each section (created, changed, left alone).
create table pack_install (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null check (length(name) between 1 and 64),
  version bigint not null check (version >= 1),
  digest text not null,
  installed_by uuid,
  installed_at timestamptz not null default now(),
  summary jsonb not null,
  primary key (tenant_id, id)
);

alter table pack_install enable row level security;
alter table pack_install force row level security;
create policy tenant_isolation on pack_install using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
