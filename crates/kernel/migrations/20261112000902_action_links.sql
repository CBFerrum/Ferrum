-- Signed action links (docs/builder/decisions.md BD-23): "a person with this link may do this one thing to this one
-- record". The link's token is 256 random bits handed out once; only its SHA-256 is kept.
create table action_link (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  digest text not null,
  entity text not null,
  record_id uuid not null,
  -- The entity's actions it may run (`view` alone: it only shows).
  actions text[] not null check (cardinality(actions) between 1 and 8),
  -- The fields its page shows, an allow-list.
  show text[] not null default '{}',
  -- Parameters the issuer fixed; the visitor cannot change them.
  params jsonb not null default '{}',
  max_uses integer not null default 1 check (max_uses between 1 and 100),
  uses integer not null default 0,
  expires_at timestamptz not null,
  -- An address or a number a one-time code goes to before the action runs.
  code_to text,
  -- Who it was issued to, as the record's history names the guest.
  recipient text check (recipient is null or length(recipient) <= 200),
  frame_origins text[] not null default '{}',
  issued_by uuid,
  created_at timestamptz not null default now(),
  revoked_at timestamptz,
  primary key (tenant_id, id),
  unique (tenant_id, digest)
);
create index action_link_record on action_link (tenant_id, entity, record_id);

alter table action_link enable row level security;
alter table action_link force row level security;
create policy tenant_isolation on action_link using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
