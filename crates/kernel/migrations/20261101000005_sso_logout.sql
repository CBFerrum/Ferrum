-- OpenID Connect back-channel logout (crates/kernel/src/sso.rs). `sso_sid` says whose a provider's
-- session is, for logout tokens that name only the session; `sso_logout_seen` keeps a token from
-- being used twice.
create table sso_sid (
  tenant_id uuid not null,
  provider_id uuid not null,
  sid text not null check (length(sid) between 1 and 255),
  user_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, provider_id, sid),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index sso_sid_created on sso_sid (created_at);

create table sso_logout_seen (
  tenant_id uuid not null references tenant (id) on delete cascade,
  provider_id uuid not null,
  jti text not null check (length(jti) between 1 and 255),
  seen_at timestamptz not null default now(),
  primary key (tenant_id, provider_id, jti)
);
create index sso_logout_seen_at on sso_logout_seen (seen_at);

alter table sso_sid enable row level security;
alter table sso_sid force row level security;
create policy tenant_isolation on sso_sid
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table sso_logout_seen enable row level security;
alter table sso_logout_seen force row level security;
create policy tenant_isolation on sso_logout_seen
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
