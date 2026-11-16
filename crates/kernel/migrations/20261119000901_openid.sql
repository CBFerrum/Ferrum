-- Ferrum as an OpenID Connect provider for the programs that run beside it (crates/kernel/src/oidc.rs; docs/api.md,
-- "Signing in to programs beside Ferrum"; docs/plan/stateless.md, D8). New tables and one new column: nothing an older
-- build reads changes, and an older build never answers for these clients (it does not know them).

-- A program an installed app runs beside Ferrum (its companion), declared by the app's own code for a workspace: a
-- confidential client whose secret the vault made as the app's own. Only the secret's SHA-256 digest is here.
create table oauth_companion (
  client_id text primary key,
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  name text not null,
  redirect_uris text[] not null check (cardinality(redirect_uris) between 1 and 10),
  secret_hash text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (tenant_id, app, name)
);

-- One sign-in to a companion, from its authorization request on: waiting while its person confirms it is them, then a
-- one-time code (a minute), then the access token's digest (five minutes). Codes and tokens are stored only as SHA-256
-- digests. Kept a day after it ends, so that a client never has a nonce taken twice.
create table oidc_code (
  code_hash text primary key,
  client_id text not null references oauth_companion(client_id) on delete cascade,
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  session_id uuid not null,
  token_version int not null,
  issuer text not null,
  redirect_uri text not null,
  code_challenge text not null,
  scope text not null,
  nonce text,
  state text not null default '',
  max_age int,
  prompt_login boolean not null default false,
  waiting boolean not null default false,
  auth_time timestamptz,
  amr text[],
  access_hash text unique,
  access_expires_at timestamptz,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz
);
create unique index oidc_code_nonce on oidc_code (client_id, nonce) where nonce is not null;

-- The subject a workspace's ID tokens name a person by: random, so that it says nothing else about them, and kept, so
-- that it never changes.
create table oidc_subject (
  tenant_id uuid not null,
  user_id uuid not null,
  subject text not null,
  primary key (tenant_id, user_id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

-- Which browser session signed in to which companion, until its app has been told that it ended
-- (`ferrum.companion.signed_out`). Gone with the session.
create table oidc_sign_in (
  tenant_id uuid not null,
  session_id uuid not null,
  client_id text not null references oauth_companion(client_id) on delete cascade,
  user_id uuid not null,
  token_version int not null,
  signed_in_at timestamptz not null default now(),
  ended_at timestamptz,
  primary key (tenant_id, session_id, client_id),
  foreign key (tenant_id, session_id) references user_session (tenant_id, id) on delete cascade
);
create index oidc_sign_in_live on oidc_sign_in (signed_in_at) where ended_at is null;

-- Only the server's system context touches these (the token endpoint runs before any workspace is known).
do $$
declare t text;
begin
  foreach t in array array['oauth_companion', 'oidc_code', 'oidc_subject', 'oidc_sign_in'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy system_only on %I using ((select ferrum_is_system())) with check ((select ferrum_is_system()))', t);
  end loop;
end $$;

-- How a browser session's person last proved who they are (an ID token's `amr`): the steps of the sign-in that opened
-- it, or the way of its last step-up. A session an older build opened has none, and its ID tokens then say nothing of it.
alter table user_session add column proof text[];
