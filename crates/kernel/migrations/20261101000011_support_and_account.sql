-- Support access a workspace gives, and every use of it (crates/kernel/src/support.rs).
alter table app_user drop constraint if exists app_user_kind_check;
alter table app_user add constraint app_user_kind_check check (kind in ('member', 'portal', 'guest', 'service', 'support'));
alter table api_key drop constraint api_key_scope_check;
alter table api_key add constraint api_key_scope_check check (scope in ('dav', 'scim', 'support'));

create table support_grant (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  level text not null check (level in ('health', 'diagnostics', 'assistance', 'incident')),
  grantee text not null check (length(grantee) between 1 and 200),
  entities text[] not null default '{}',
  can_write boolean not null default false,
  reason text not null check (length(reason) between 1 and 500),
  user_id uuid not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  revoked_at timestamptz,
  revoked_by uuid,
  primary key (tenant_id, id)
);
create unique index support_grant_user on support_grant (tenant_id, user_id);

create table support_access (
  tenant_id uuid not null,
  grant_id uuid not null,
  seq bigint generated always as identity,
  at timestamptz not null default now(),
  method text not null,
  path text not null,
  primary key (tenant_id, seq),
  foreign key (tenant_id, grant_id) references support_grant (tenant_id, id) on delete cascade
);
create index support_access_grant on support_access (tenant_id, grant_id, seq desc);

-- Where a workspace's account stands (crates/kernel/src/account.rs). Nothing here touches its data.
alter table tenant add column account_state text not null default 'active' check (account_state in ('active', 'grace', 'restricted'));
alter table tenant add column account_note text;
alter table tenant add column quotas jsonb not null default '{}';

-- Usage as it stood at the end of each day, so a bill can be explained later.
create table usage_day (
  tenant_id uuid not null references tenant (id) on delete cascade,
  day date not null,
  meter text not null,
  quantity bigint not null,
  primary key (tenant_id, day, meter)
);

do $$
declare t text;
begin
  foreach t in array array['support_grant', 'support_access', 'usage_day'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
