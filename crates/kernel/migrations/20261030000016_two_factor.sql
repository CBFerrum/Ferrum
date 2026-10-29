-- Two-factor sign-in with an authenticator app (TOTP): the secret sealed with the server key, the
-- last time step accepted (each code works once), and single-use recovery codes kept as hashes.
alter table app_user
  add column totp_secret text,
  add column totp_enabled_at timestamptz,
  add column totp_last_step bigint;

create table user_recovery_code (
  tenant_id uuid not null,
  user_id uuid not null,
  code_hash text not null,
  used_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, user_id, code_hash),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
alter table user_recovery_code enable row level security;
alter table user_recovery_code force row level security;
create policy tenant_isolation on user_recovery_code
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
