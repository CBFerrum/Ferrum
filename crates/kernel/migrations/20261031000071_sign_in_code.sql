-- Codes sent by email to sign in with (docs/plan/apps-ui.md §9.1): portal people have no password.
-- Only a hash is kept. A code lasts ten minutes, takes five wrong tries and works once; sending a
-- new one ends the ones before.
create table sign_in_code (
  id uuid primary key default gen_random_uuid(),
  tenant_id uuid not null,
  user_id uuid not null,
  code_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  attempts int not null default 0,
  used_at timestamptz,
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

create index sign_in_code_user on sign_in_code (tenant_id, user_id, created_at desc);
create index sign_in_code_expiry on sign_in_code (expires_at);

alter table sign_in_code enable row level security;
alter table sign_in_code force row level security;
create policy tenant_isolation on sign_in_code
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
