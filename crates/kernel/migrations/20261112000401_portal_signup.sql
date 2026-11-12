-- Outside people signing themselves up to a workspace's portal (docs/api.md, "Public sign-up"; BD-02).
-- A portal role's `public_signup` turns it on: {domains: [...], passes: [...]}; null (the default) leaves it off.
alter table role add column public_signup jsonb;

-- Each request to sign up, kept for the limits (per address and per network address, across replicas) and, when it
-- sent a code, until the code is confirmed: nobody is made a person of the workspace before that.
create table portal_signup (
  id uuid primary key,
  tenant_id uuid not null references tenant (id) on delete cascade,
  email citext not null,
  full_name text not null,
  role text not null,
  passed jsonb not null default '{}',
  network text not null,
  code_hash text,
  attempts int not null default 0,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz
);

create index portal_signup_email on portal_signup (tenant_id, email, created_at desc);
create index portal_signup_network on portal_signup (tenant_id, network, created_at desc);
create index portal_signup_expiry on portal_signup (created_at);

alter table portal_signup enable row level security;
alter table portal_signup force row level security;
create policy tenant_isolation on portal_signup
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
