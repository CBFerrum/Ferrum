-- SCIM provisioning (RFC 7643, 7644; docs/identity-lifecycle.md). A person keeps their row when a
-- provider deletes them: their name stays on what they did, and `scim_deleted_at` hides them from
-- SCIM until the provider creates them again. Groups are the provider's: they grant the roles an
-- administrator maps them to, and `via_scim` marks the grants that leave when the membership does,
-- so a role given by hand survives.
alter table app_user
  add column scim_external_id text check (length(scim_external_id) <= 255),
  add column scim_deleted_at timestamptz;
create unique index app_user_scim_external on app_user (tenant_id, scim_external_id) where scim_external_id is not null;

alter table user_role add column via_scim boolean not null default false;

create table scim_group (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  display_name text not null check (length(display_name) between 1 and 200),
  external_id text check (length(external_id) <= 255),
  roles text[] not null default '{}',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, display_name)
);
create unique index scim_group_external on scim_group (tenant_id, external_id) where external_id is not null;

create table scim_group_member (
  tenant_id uuid not null,
  group_id uuid not null,
  user_id uuid not null,
  primary key (tenant_id, group_id, user_id),
  foreign key (tenant_id, group_id) references scim_group (tenant_id, id) on delete cascade,
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index scim_group_member_user on scim_group_member (tenant_id, user_id);

do $$
declare t text;
begin
  foreach t in array array['scim_group', 'scim_group_member'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;

alter table api_key drop constraint api_key_scope_check;
alter table api_key add constraint api_key_scope_check check (scope in ('dav', 'scim'));
