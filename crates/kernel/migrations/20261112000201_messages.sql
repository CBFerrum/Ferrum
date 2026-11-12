-- Message templates and channels (crates/kernel/src/messages.rs, docs/builder/decisions.md BD-20).

-- A workspace's own templates, and its edited copies of packages' defaults: `base` is the digest of the default an
-- edit started from, so that a release changing that default is told, never written over the edit.
create table message_template (
  tenant_id uuid not null references tenant (id) on delete cascade,
  key text not null check (length(key) between 1 and 100),
  def jsonb not null,
  base text,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, key)
);

-- A text-message or chat-app gateway a workspace connects: its request described, never named in code; its secrets
-- sealed (`sealed`, never read back). One enabled for each kind. `refused` counts inbound calls refused.
create table message_channel (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  kind text not null check (kind in ('text', 'chat')),
  name text not null check (length(name) between 1 and 100),
  description jsonb not null,
  sealed text,
  enabled boolean not null default true,
  refused integer not null default 0,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create unique index message_channel_kind on message_channel (tenant_id, kind) where enabled;

-- Every send: what, to whom, on which channel, and how it went. `recipient` is an address or a number in
-- international form (erasure reaches it); `pending` what goes out, held only until it has; `content` what the test
-- channel delivered (a code masked).
create table message_send (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  template text not null,
  channel text not null check (channel in ('email', 'text', 'chat', 'inapp')),
  channel_id uuid,
  recipient text,
  user_id uuid,
  entity text,
  record_id uuid,
  language text,
  state text not null check (state in ('queued', 'held', 'sent', 'delivered', 'failed', 'skipped')),
  reason text,
  provider_id text,
  error text,
  pending jsonb,
  content jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index message_send_record on message_send (tenant_id, entity, record_id, created_at desc);
create index message_send_recipient on message_send (tenant_id, recipient, created_at desc);
create index message_send_provider on message_send (tenant_id, channel_id, provider_id);

-- One-time codes: a salted digest, never the code.
create table message_code (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  purpose text not null,
  recipient text not null,
  digest text not null,
  attempts integer not null default 0,
  expires_at timestamptz not null,
  used_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index message_code_recipient on message_code (tenant_id, recipient, created_at desc);

-- Opt-outs per address or number and channel: the consent records there are, a channel more. What was recorded
-- before is about e-mail.
alter table contact_consent add column channel text not null default 'email' check (channel in ('email', 'text', 'chat'));
alter table contact_consent drop constraint contact_consent_pkey;
alter table contact_consent add primary key (tenant_id, email, purpose, channel);

do $$
declare t text;
begin
  foreach t in array array['message_template', 'message_channel', 'message_send', 'message_code'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
                   'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
