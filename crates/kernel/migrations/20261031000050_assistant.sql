-- The assistant's conversations (docs/plan/apps-ui.md D2, §11.4; the server's assistant.rs): each
-- person's threads, and their messages exactly as the model's API takes them back. Messages are
-- only ever appended: the model's thinking stays valid only in the conversation that produced it.
create table assistant_thread (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  user_id uuid not null,
  title text not null default '',
  -- idle: nothing under way. running: a turn is (since running_since, on some replica; one that
  -- has not moved for a while is taken to have stopped). waiting: changes the assistant proposed
  -- wait for the person's decision (pending: the calls, and the results of those that ran).
  state text not null default 'idle' check (state in ('idle', 'running', 'waiting')),
  running_since timestamptz,
  stop_requested boolean not null default false,
  pending jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

create index assistant_thread_mine on assistant_thread (tenant_id, user_id, updated_at desc);

create table assistant_message (
  tenant_id uuid not null,
  thread_id uuid not null,
  seq int not null,
  role text not null check (role in ('user', 'assistant')),
  content jsonb not null,
  -- The model's report for an assistant message (tokens, the model that answered).
  usage jsonb,
  created_at timestamptz not null default now(),
  primary key (tenant_id, thread_id, seq),
  foreign key (tenant_id, thread_id) references assistant_thread (tenant_id, id) on delete cascade
);

-- A person's calls to the model in the last hour (their allowance): one assistant message each.
create index assistant_message_calls on assistant_message (tenant_id, created_at) where role = 'assistant';

alter table assistant_thread enable row level security;
alter table assistant_thread force row level security;
create policy tenant_isolation on assistant_thread
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table assistant_message enable row level security;
alter table assistant_message force row level security;
create policy tenant_isolation on assistant_message
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
