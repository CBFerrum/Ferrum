-- Connections to outside services (crates/kernel/src/connections, docs/builder/decisions.md BD-29 to BD-33): a
-- described HTTP service a workspace calls, is called by, pulls from and signs in to. No provider is named in code.

-- A connection: its description (address, authentication, operations, inbound, pulls), its secrets sealed with the
-- server's key (`sealed`, never read back), off until an administrator turns it on. `source` says where the
-- description came from (`pack:<name>`, `package:<name>`), null for one made here. `refused` counts inbound calls
-- refused; `debug_until` turns on a short-lived, redacted capture of requests and answers in the call log.
create table conn (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  name text not null check (name ~ '^[a-z][a-z0-9_]{0,62}$'),
  label text not null check (length(label) between 1 and 100),
  description jsonb not null,
  sealed text,
  enabled boolean not null default false,
  source text,
  refused integer not null default 0,
  debug_until timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, name)
);

-- Tokens a person's sign-in at a provider gave (BD-32): the workspace's account (`user_id` nil) or a person's own.
-- `sealed` holds the access and refresh tokens, sealed; nothing reads them back out. `told` says the owner heard that
-- it needs reconnecting, so they hear it once.
create table conn_token (
  tenant_id uuid not null,
  conn_id uuid not null,
  user_id uuid not null,
  sealed text not null,
  expires_at timestamptz,
  account text check (account is null or length(account) <= 200),
  status text not null default 'connected' check (status in ('connected', 'needs_reconnecting')),
  told boolean not null default false,
  connected_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, conn_id, user_id),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

-- A sign-in under way: the SHA-256 of its state (the state itself goes to the browser only), bound to the person and
-- their browser session; the PKCE verifier sealed. Used once, within ten minutes.
create table conn_signin (
  state_digest text primary key,
  tenant_id uuid not null,
  conn_id uuid not null,
  user_id uuid not null,
  owner_id uuid not null,
  session_id uuid not null,
  verifier text not null,
  expires_at timestamptz not null,
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

-- Every call: when, which operation, how it went and how long it took, the record it was about. No bodies, unless the
-- debug capture was on (redacted). A call that changes something with a key keeps its mapped result, so the same key
-- again gets it without calling twice.
create table conn_call (
  tenant_id uuid not null,
  id uuid not null,
  conn_id uuid not null,
  operation text not null,
  at timestamptz not null default now(),
  outcome text not null check (outcome in ('ok', 'failed', 'temporary', 'refused')),
  status integer,
  duration_ms integer not null default 0,
  entity text,
  record_id uuid,
  error text,
  key text,
  input_digest text,
  result jsonb,
  debug jsonb,
  primary key (tenant_id, id),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);
create index conn_call_at on conn_call (tenant_id, conn_id, at desc);
create unique index conn_call_once on conn_call (tenant_id, conn_id, operation, key) where key is not null and outcome = 'ok';

-- Calls from outside, accepted: one row per delivery (the caller's id, else a digest of what it signed), processed
-- once by a job; the payload is dropped once it is.
create table conn_inbound (
  tenant_id uuid not null,
  conn_id uuid not null,
  delivery text not null,
  received_at timestamptz not null default now(),
  state text not null default 'accepted' check (state in ('accepted', 'done', 'failed')),
  payload jsonb,
  error text,
  primary key (tenant_id, conn_id, delivery),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

-- A pull's place between runs (BD-31).
create table conn_pull (
  tenant_id uuid not null,
  conn_id uuid not null,
  name text not null,
  cursor text,
  next_run timestamptz not null default now(),
  last_run timestamptz,
  last_error text,
  primary key (tenant_id, conn_id, name),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

do $$
declare t text;
begin
  foreach t in array array['conn', 'conn_token', 'conn_signin', 'conn_call', 'conn_inbound', 'conn_pull'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
                   'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
