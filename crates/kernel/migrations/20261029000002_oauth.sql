-- OAuth 2.1 for MCP and other API clients: self-registered public clients, one-time PKCE codes,
-- rotating refresh tokens. Secrets are stored as SHA-256 hashes only.
create table oauth_client (
  id text primary key,
  name text not null,
  redirect_uris text[] not null check (cardinality(redirect_uris) between 1 and 10),
  created_at timestamptz not null default now(),
  last_used_at timestamptz
);

create table oauth_code (
  code_hash text primary key,
  client_id text not null references oauth_client(id) on delete cascade,
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  redirect_uri text not null,
  code_challenge text not null,
  scope text not null,
  expires_at timestamptz not null,
  used_at timestamptz
);

create table oauth_refresh (
  token_hash text primary key,
  client_id text not null references oauth_client(id) on delete cascade,
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  scope text not null,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  created_at timestamptz not null default now()
);
create index oauth_refresh_owner on oauth_refresh (client_id, tenant_id, user_id) where revoked_at is null;

-- Only the server's system context touches these tables (token endpoints run before any tenant is known).
do $$
declare t text;
begin
  foreach t in array array['oauth_client', 'oauth_code', 'oauth_refresh'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy system_only on %I using ((select ferrum_is_system())) with check ((select ferrum_is_system()))', t);
  end loop;
end $$;
