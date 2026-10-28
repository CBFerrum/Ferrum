-- Secret field values (passwords, API keys), sealed with AES-256-GCM and bound to their tenant,
-- entity, record and field. Never part of documents, events, the audit trail or exports.
create table record_secret (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  field text not null,
  sealed text not null,
  key_id text not null,
  updated_at timestamptz not null default now(),
  updated_by uuid,
  primary key (tenant_id, entity, record_id, field)
);

alter table record_secret enable row level security;
alter table record_secret force row level security;
create policy tenant_isolation on record_secret using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
