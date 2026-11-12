-- Origins that may frame one of an app's public pages (docs/builder/decisions.md BD-24): a form embedded in a
-- website. None by default; a page with no row stays unframeable.
create table public_frame (
  tenant_id uuid not null references tenant (id) on delete cascade,
  app text not null,
  path text not null check (length(path) between 1 and 300),
  origins text[] not null check (cardinality(origins) between 1 and 10),
  primary key (tenant_id, app, path)
);
alter table public_frame enable row level security;
alter table public_frame force row level security;
create policy tenant_isolation on public_frame using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
