-- Agents (gapclose/design/A2-A3-agents.md, AG-1; crates/kernel/src/agent.rs): an agent is a record one member owns. On
-- every request its context is its owner's as they are now, narrowed by its grants: never more than its owner, never an
-- administrator. Until agents open (AG-9) the build refuses every agent credential.
create table agent (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  owner_id uuid not null,
  name text not null check (length(btrim(name)) between 1 and 100),
  state text not null default 'active' check (state in ('active', 'paused', 'revoked')),
  -- Moves on with every change of what it may do, never with its state: a request asked under an earlier revision
  -- cannot be approved.
  revision int not null default 1 check (revision >= 1),
  grants jsonb not null,                       -- {"entities": {...}, "operations": [...]}
  classes jsonb not null default '{}',         -- only what differs from the workspace's
  -- The classes an administrator set: each stands as set. A class only its owner set counts at least as the workspace's
  -- does now, so a workspace made stricter later is never undercut by an owner's earlier choice.
  admin_classes text[] not null default '{}',
  limits jsonb not null default '{}',
  data_policy jsonb not null default '{}',     -- crate::ctx::Egress
  approver_role text,                          -- checked in code; a role gone approves nobody
  networks text[],                             -- its own; null: the workspace's staff networks hold it
  expires_at timestamptz,
  -- No foreign key: OAuth clients are the installation's, and a move carries agents where they name nothing.
  oauth_client text check (length(oauth_client) <= 200),
  oauth_redirect text check (length(oauth_redirect) <= 2000),   -- the return address it was last connected with
  created_by uuid not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  last_used_at timestamptz,
  last_notice_at timestamptz,                  -- approvers told of its requests at most once in 10 minutes
  paused_by uuid,
  revoked_at timestamptz,
  revoked_by uuid,
  primary key (tenant_id, id),
  unique (tenant_id, id, owner_id),            -- what credentials point at: a credential's person is its agent's owner
  foreign key (tenant_id, owner_id) references app_user(tenant_id, id) on delete cascade,
  check ((state = 'revoked') = (revoked_at is not null)),
  check (jsonb_typeof(grants) = 'object' and jsonb_typeof(classes) = 'object'
         and jsonb_typeof(limits) = 'object' and jsonb_typeof(data_policy) = 'object')
);
create unique index agent_name on agent (tenant_id, owner_id, lower(btrim(name))) where state <> 'revoked';
create unique index agent_oauth on agent (tenant_id, owner_id, oauth_client) where state <> 'revoked' and oauth_client is not null;
create index agent_owner on agent (tenant_id, owner_id);
-- Whose agent it is, and who made it, never change, whatever code runs.
create trigger agent_owner_kept before update of tenant_id, owner_id, created_by on agent for each row
  when ((old.tenant_id, old.owner_id, old.created_by) is distinct from (new.tenant_id, new.owner_id, new.created_by))
  execute function ferrum_forbid_change();

-- An agent's keys are rows of api_key naming it: their person is the agent's owner, by the key below, and they are never
-- narrowed to one use.
alter table api_key add column agent_id uuid;
alter table api_key add constraint api_key_agent -- window: safe, an older build's keys name no agent, which the key allows
  foreign key (tenant_id, agent_id, user_id) references agent(tenant_id, id, owner_id) on delete cascade not valid;
alter table api_key validate constraint api_key_agent;
alter table api_key add constraint api_key_agent_unscoped -- window: safe, an older build's keys name no agent, which the check allows
  check (agent_id is null or scope is null) not valid;
alter table api_key validate constraint api_key_agent_unscoped;
create index api_key_agent_of -- window: safe, keys are few (a person's handful): built in a moment
  on api_key (tenant_id, agent_id) where agent_id is not null;

-- An agent's day, for its limits and the console.
create table agent_day (
  tenant_id uuid not null,
  agent_id uuid not null,
  day date not null,
  calls int not null default 0,
  refused int not null default 0,
  asked int not null default 0,                -- requests made; cancelling one does not give it back
  upload_bytes bigint not null default 0,
  primary key (tenant_id, agent_id, day),
  foreign key (tenant_id, agent_id) references agent(tenant_id, id) on delete cascade
);

do $$
declare t text;
begin
  foreach t in array array['agent', 'agent_day'] loop
    execute format('alter table %s enable row level security', t);
    execute format('alter table %s force row level security', t);
    execute format('create policy tenant_isolation on %s using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
